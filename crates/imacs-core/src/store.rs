//! Remembers Claude sessions IMACS has seen so they can be resumed after a restart.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};

const MAX_RECORDS: usize = 60;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub claude_session_id: String,
    pub agent: String,
    pub cwd: PathBuf,
    pub title: String,
    pub last_prompt: Option<String>,
    pub updated: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Store {
    pub sessions: Vec<SessionRecord>,
    /// Projects open when IMACS last ran.
    #[serde(default)]
    pub projects: Vec<PathBuf>,
    #[serde(default)]
    pub recent_projects: Vec<PathBuf>,
}

impl Store {
    pub fn load(path: &Path) -> Store {
        std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
    }

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

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, prompt: Option<&str>) -> SessionRecord {
        SessionRecord {
            claude_session_id: id.into(),
            agent: "claude".into(),
            cwd: PathBuf::from("/r"),
            title: id.into(),
            last_prompt: prompt.map(str::to_string),
            updated: 0,
        }
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

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("s.json");
        store.save(&file).unwrap();
        assert_eq!(Store::load(&file).sessions, store.sessions);
    }
}
