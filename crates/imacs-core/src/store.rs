//! Remembers Claude sessions IMACS has seen so they can be resumed after a restart.

use std::path::{Path, PathBuf};
#[cfg(feature = "native")]
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::ChiefDock;

#[cfg(feature = "native")]
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[cfg(feature = "native")]
const MAX_RECORDS: usize = 60;

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
    #[serde(default)]
    pub chief_dock: Option<ChiefDock>,
    #[serde(default)]
    pub chief_width: Option<f32>,
    pub sessions: Vec<SessionRecord>,
    /// Projects open when IMACS last ran.
    #[serde(default)]
    pub projects: Vec<PathBuf>,
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
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, path)?;
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
