//! Remembers Claude sessions ATOMICS has seen so they can be resumed after a restart.

use std::path::{Path, PathBuf};
#[cfg(feature = "native")]
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::ChiefDock;

#[cfg(feature = "native")]
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[cfg(feature = "native")]
const MAX_RECORDS: usize = 60;

/// Create private state directories, tightening existing directories only when owned by this user.
#[cfg(feature = "native")]
pub fn private_dir(path: &Path) -> std::io::Result<()> {
    state_dir(path, false)
}

/// Loading readable state must not fail just because this filesystem cannot change its mode.
#[cfg(feature = "native")]
pub fn readable_state_dir(path: &Path) -> std::io::Result<()> {
    state_dir(path, true)
}

#[cfg(feature = "native")]
fn state_dir(path: &Path, reading: bool) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    let metadata = std::fs::metadata(path)?;
    // Shared state may be readable/writable without being owned by this user.
    // SAFETY: geteuid has no preconditions.
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.permissions().mode() & 0o7777 == 0o700 {
        return Ok(());
    }
    match std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)) {
        Err(error)
            if reading
                && matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied
                        | std::io::ErrorKind::ReadOnlyFilesystem
                        | std::io::ErrorKind::Unsupported
                ) =>
        {
            Ok(())
        }
        result => result,
    }
}

/// Publishes `bytes` at `path` as [`write_private`] does, on the writer's own thread: a caller that holds a lock others
/// wait on never waits for the disk. Files are written in the order they were first queued, and a path queued again
/// before it is written is written once, with the latest bytes. `private` is a folder to make private first. A crash
/// can lose writes still queued; [`flush_writes`] waits for them.
#[cfg(feature = "native")]
pub fn write_later(path: PathBuf, bytes: Vec<u8>, private: Option<PathBuf>) {
    later::queue(path, bytes, private);
}

/// Waits until everything [`write_later`] queued so far is on disk. Reading these files back in this process, and
/// writing them by other means, comes after it.
#[cfg(feature = "native")]
pub fn flush_writes() {
    later::flush();
}

/// What [`write_later`] could not write since this was last asked, as `path: error`.
#[cfg(feature = "native")]
pub fn take_write_failures() -> Vec<String> {
    later::failures()
}

#[cfg(feature = "native")]
mod later {
    use std::collections::{HashMap, VecDeque};
    use std::path::PathBuf;
    use std::sync::{Condvar, LazyLock, Mutex, MutexGuard};

    #[derive(Default)]
    struct Queue {
        order: VecDeque<PathBuf>,
        files: HashMap<PathBuf, (Vec<u8>, Option<PathBuf>)>,
        writing: bool,
        started: bool,
        failures: Vec<String>,
    }

    static QUEUE: LazyLock<(Mutex<Queue>, Condvar)> = LazyLock::new(Default::default);

    fn lock() -> MutexGuard<'static, Queue> {
        QUEUE.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn queue(path: PathBuf, bytes: Vec<u8>, private: Option<PathBuf>) {
        let mut queue = lock();
        if !queue.started {
            queue.started = std::thread::Builder::new().name("atomics-writer".into()).spawn(run).is_ok();
            if !queue.started {
                drop(queue);
                write(&path, &bytes, private.as_deref());
                return;
            }
        }
        if queue.files.insert(path.clone(), (bytes, private)).is_none() {
            queue.order.push_back(path);
        }
        QUEUE.1.notify_all();
    }

    pub(super) fn flush() {
        let mut queue = lock();
        while queue.writing || !queue.order.is_empty() {
            queue = QUEUE.1.wait(queue).unwrap_or_else(|e| e.into_inner());
        }
    }

    pub(super) fn failures() -> Vec<String> {
        std::mem::take(&mut lock().failures)
    }

    fn run() {
        let mut queue = lock();
        loop {
            let Some(path) = queue.order.pop_front() else {
                queue = QUEUE.1.wait(queue).unwrap_or_else(|e| e.into_inner());
                continue;
            };
            let (bytes, private) = queue.files.remove(&path).expect("queued with its bytes");
            queue.writing = true;
            drop(queue);
            let failed = write(&path, &bytes, private.as_deref());
            queue = lock();
            queue.writing = false;
            queue.failures.extend(failed);
            QUEUE.1.notify_all();
        }
    }

    fn write(path: &std::path::Path, bytes: &[u8], private: Option<&std::path::Path>) -> Option<String> {
        let written = private.map_or(Ok(()), super::private_dir).and_then(|()| super::write_private(path, bytes));
        let error = written.err()?;
        let failure = format!("{}: {error}", crate::paths::display(path));
        eprintln!("ATOMICS: writing {failure}");
        Some(failure)
    }
}

/// Publish a complete private file. Each writer owns its temporary file, including across processes.
#[cfg(feature = "native")]
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    private_dir(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    #[serde(default)]
    pub chief: bool,
    #[serde(default)]
    pub pane: Option<String>,
    /// The conversation reported by the provider. Older snapshots used an empty string for an unknown id.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub claude_session_id: String,
    pub agent: String,
    pub cwd: PathBuf,
    pub title: String,
    pub last_prompt: Option<String>,
    pub updated: u64,
    /// The sandbox a sandboxed session runs in; resuming reattaches to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    /// Whether ATOMICS started the session in a worktree it prepared. Older records omit this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_worktree: Option<bool>,
    /// Its title came from its first prompt or the user, so resuming keeps it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub named: bool,
    /// Claude has not saved this conversation yet, as far as ATOMICS saw: Claude saves one when its first
    /// prompt is submitted, which a first-run screen such as the folder-trust question holds back. Resuming
    /// it starts it again under the same id, with the prompt it was started with. `None` once a turn started,
    /// and for every record written before this was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsaved: Option<Unsaved>,
}

/// A conversation that was started but not saved yet: what its first turn was to be.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Unsaved {
    /// The prompt it was started with, before ATOMICS added the repository's own instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

/// The sessions running when this was last written, and the version that ran them. ATOMICS keeps it current
/// while it runs and empties it on a quit, so a start that finds sessions here without a handoff knows the
/// last run ended some other way (its service was restarted, or it was killed or crashed) and resumes them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Running {
    pub version: String,
    pub sessions: Vec<SessionRecord>,
}

/// One-shot handoff, written before quitting and consumed by the new process.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RestartState {
    pub old_version: String,
    pub reason: Option<String>,
    pub sessions: Vec<SessionRecord>,
    pub chief_open: bool,
    pub active: Option<String>,
    pub selected_plan: Option<String>,
    pub plan_view: bool,
    /// The Ungrouped tab was on screen.
    #[serde(default)]
    pub ungrouped: bool,
    pub diff_open: bool,
    pub diff_root: Option<PathBuf>,
    pub view: Option<PathBuf>,
    pub split: Option<f32>,
    pub diff_split: Option<crate::layout::DiffSplit>,
    pub panel: String,
    pub window_size: Option<[f32; 2]>,
    pub maximized: bool,
    /// Made at start from [`Store::running`]: nobody handed off, so there is no layout to restore.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unplanned: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Store {
    /// The saved panel choice applies while the configured selection is the one it replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chief_selection: Option<(crate::projection::ChiefSelection, crate::projection::ChiefSelection)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chief_history: Vec<crate::projection::ChiefConversation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chief_mask_fingerprints: Vec<crate::tickets::MaskFingerprint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart: Option<RestartState>,
    /// A chief restart handoff that has yet to establish a new provider connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chief_recovery: Option<SessionRecord>,
    /// This install started with no config and nobody has finished or skipped the setup screen yet.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub setup_pending: bool,
    /// The connections someone said will wait, by id: they stay that way through restarts and updates.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub setup_later: Vec<String>,
    #[serde(default)]
    pub chief_dock: Option<ChiefDock>,
    #[serde(default)]
    pub chief_width: Option<f32>,
    /// The view the desktop window's switcher last showed beside the agents: `board`, `diff` or `tickets`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel_view: Option<String>,
    /// The desktop window's own choice of simple or full layout; `None` follows `[ui] mode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_mode: Option<crate::config::UiMode>,
    /// The sidebar's folded sections the desktop window has open, by name (`agents`, `explorer`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sidebar_open: Vec<String>,
    pub sessions: Vec<SessionRecord>,
    /// Projects open when ATOMICS last ran.
    #[serde(default)]
    pub projects: Vec<PathBuf>,
    /// The workspace open when ATOMICS last ran, by name: a start that names none opens it again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default)]
    pub recent_workspaces: Vec<String>,
    #[serde(default)]
    pub recent_projects: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running: Option<Running>,
    /// Explicit host declarations for the sessions resumed by a restart.
    #[serde(
        default,
        skip_serializing_if = "std::collections::HashMap::is_empty",
        serialize_with = "save_previews",
        deserialize_with = "load_previews"
    )]
    pub declared_previews: std::collections::HashMap<String, Vec<crate::preview::Preview>>,
}

fn load_previews<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<std::collections::HashMap<String, Vec<crate::preview::Preview>>, D::Error> {
    let mut declarations =
        std::collections::HashMap::<String, Vec<crate::preview::Preview>>::deserialize(deserializer)?;
    for preview in declarations.values_mut().flatten() {
        *preview = preview.for_storage();
    }
    Ok(declarations)
}

fn save_previews<S: serde::Serializer>(
    declarations: &std::collections::HashMap<String, Vec<crate::preview::Preview>>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.collect_map(declarations.iter().map(|(session, previews)| {
        (session, previews.iter().map(crate::preview::Preview::for_storage).collect::<Vec<_>>())
    }))
}

impl Store {
    #[cfg(feature = "native")]
    pub fn load(path: &Path) -> Store {
        flush_writes();
        std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
    }

    #[cfg(feature = "native")]
    pub fn save(&self, path: &Path) -> Result<()> {
        flush_writes();
        write_private(path, &serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    /// [`Self::save`] through [`write_later`]: only serialising waits.
    #[cfg(feature = "native")]
    pub fn save_later(&self, path: &Path) -> Result<()> {
        write_later(path.to_path_buf(), serde_json::to_vec_pretty(self)?, None);
        Ok(())
    }

    /// Inserts or refreshes a record, newest first.
    #[cfg(feature = "native")]
    pub fn upsert(&mut self, mut record: SessionRecord) {
        record.updated = now();
        if let Some(existing) = self.sessions.iter().find(|r| r.claude_session_id == record.claude_session_id)
            && record.last_prompt.is_none()
        {
            record.last_prompt = existing.last_prompt.clone();
        }
        self.sessions.retain(|r| r.claude_session_id != record.claude_session_id);
        self.sessions.insert(0, record);
        self.sessions.truncate(MAX_RECORDS);
    }
}

impl Store {
    /// Remembers a project as recently opened, newest first.
    pub fn touch_project(&mut self, root: &Path) {
        self.recent_projects.retain(|p| p != root);
        self.recent_projects.insert(0, root.to_path_buf());
        self.recent_projects.truncate(15);
    }
}

#[cfg(feature = "native")]
pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;

    #[test]
    fn later_writes_keep_the_latest_bytes_and_flush_waits_for_them() {
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = (dir.path().join("state/first.json"), dir.path().join("second.json"));
        for n in 0..50 {
            write_later(first.clone(), format!("{n}").into_bytes(), Some(dir.path().join("state")));
            write_later(second.clone(), format!("{}", n * 2).into_bytes(), None);
        }
        flush_writes();
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "49");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "98");
        let saved = Store { recent_projects: vec![PathBuf::from("/project")], ..Default::default() };
        saved.save_later(&second).unwrap();
        assert_eq!(Store::load(&second).recent_projects, saved.recent_projects, "a read waits for what was queued");
        assert!(take_write_failures().iter().all(|failure| !failure.contains("first.json")));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn review_p7_readable_state_survives_a_filesystem_that_refuses_chmod() {
        use std::os::unix::fs::PermissionsExt;
        // This process owns the procfs directory, but procfs refuses chmod even for its owner.
        let path = Path::new("/proc/self/fd");
        let mode = std::fs::metadata(path).unwrap().permissions().mode();
        readable_state_dir(path).unwrap();
        assert!(std::fs::read_dir(path).is_ok());
        assert_eq!(std::fs::metadata(path).unwrap().permissions().mode(), mode);
        assert_eq!(private_dir(path).unwrap_err().raw_os_error(), Some(libc::EPERM));
    }

    #[test]
    fn audit_p7_concurrent_saves_publish_complete_private_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_support::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir(&state).unwrap();
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o777)).unwrap();
        let path = state.join("sessions.json");
        std::fs::write(&path, b"old public file").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        // The formerly shared temporary name must not be opened, followed or removed.
        let victim = dir.path().join("untouched");
        std::fs::write(&victim, b"original").unwrap();
        std::os::unix::fs::symlink(&victim, path.with_extension("json.tmp")).unwrap();
        std::thread::scope(|scope| {
            for i in 0..8 {
                let path = &path;
                scope.spawn(move || {
                    let store = Store {
                        sessions: vec![record(&i.to_string(), Some(&"prompt".repeat(1000)))],
                        ..Store::default()
                    };
                    for _ in 0..4 {
                        store.save(path).unwrap();
                        let loaded: Store = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
                        assert_eq!(loaded.sessions.len(), 1);
                        assert_eq!(loaded.sessions[0].last_prompt.as_ref().unwrap().len(), 6000);
                    }
                });
            }
        });
        assert_eq!(std::fs::read(&victim).unwrap(), b"original");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&state).unwrap().permissions().mode() & 0o777, 0o700);
    }

    fn record(id: &str, prompt: Option<&str>) -> SessionRecord {
        SessionRecord {
            chief: false,
            pane: None,
            claude_session_id: id.into(),
            agent: "claude".into(),
            cwd: PathBuf::from("/r"),
            title: id.into(),
            last_prompt: prompt.map(str::to_string),
            updated: 0,
            sandbox: None,
            managed_worktree: None,
            named: false,
            unsaved: None,
        }
    }

    #[test]
    fn previously_saved_preview_labels_are_removed_on_load() {
        let mut old = serde_json::to_value(Store::default()).unwrap();
        old["declared_previews"] = serde_json::json!({"web":[{"port":5173,"label":"test-share-code","path":"/demo"}]});
        let store: Store = serde_json::from_value(old).unwrap();
        assert_eq!(store.declared_previews["web"][0].title(), "Port 5173");
    }

    #[test]
    fn chief_preferences_survive_restart_and_old_stores_load() {
        let dir = crate::test_support::tempdir().unwrap();
        let file = dir.path().join("sessions.json");
        let store = Store { chief_dock: Some(ChiefDock::Docked), chief_width: Some(720.0), ..Store::default() };
        store.save(&file).unwrap();
        let loaded = Store::load(&file);
        assert_eq!(loaded.chief_dock, store.chief_dock);
        assert_eq!(loaded.chief_width, store.chief_width);
        let old: Store = serde_json::from_str(r#"{"sessions":[]}"#).unwrap();
        assert_eq!(old.chief_dock, None);
        assert_eq!(old.chief_width, None);
    }

    #[test]
    fn upsert_moves_to_front_and_keeps_last_prompt() {
        let mut store = Store::default();
        store.upsert(record("a", Some("first")));
        store.upsert(record("b", None));
        store.upsert(record("a", None));
        assert_eq!(store.sessions[0].claude_session_id, "a");
        assert_eq!(store.sessions[0].last_prompt.as_deref(), Some("first"));
        assert_eq!(store.sessions.len(), 2);

        let dir = crate::test_support::tempdir().unwrap();
        let file = dir.path().join("s.json");
        store.save(&file).unwrap();
        assert_eq!(Store::load(&file).sessions, store.sessions);
    }
}
