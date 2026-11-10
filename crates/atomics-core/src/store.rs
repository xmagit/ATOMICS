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

/// Create private state directories, also tightening an existing directory's mode.
#[cfg(feature = "native")]
pub fn private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
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
    pub claude_session_id: String,
    pub agent: String,
    pub cwd: PathBuf,
    pub title: String,
    pub last_prompt: Option<String>,
    pub updated: u64,
    /// The sandbox a sandboxed session runs in; resuming reattaches to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    /// Its title came from its first prompt or the user, so resuming keeps it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub named: bool,
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
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart: Option<RestartState>,
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
    pub sessions: Vec<SessionRecord>,
    /// Projects open when ATOMICS last ran.
    #[serde(default)]
    pub projects: Vec<PathBuf>,
    /// The workspace open when ATOMICS last ran, by name: a start that names none opens it again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default)]
    pub recent_projects: Vec<PathBuf>,
}

impl Store {
    #[cfg(feature = "native")]
    pub fn load(path: &Path) -> Store {
        std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
    }

    #[cfg(feature = "native")]
    pub fn save(&self, path: &Path) -> Result<()> {
        write_private(path, &serde_json::to_vec_pretty(self)?)?;
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
            named: false,
        }
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
