//! Durable, per-plan conversation state. Kept outside Plan and the chief's plan JSON.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tickets::Vars;

pub const MAX_PENDING: usize = 200;
pub const MAX_SEEN: usize = 500;
pub const MAX_MIRROR: usize = 50;
pub const DAILY_POSTS: u64 = 1000;
pub const WITHHELD_TEXT: &str = "(withheld: the ticket system refused this text)";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteState {
    pub conversation: Option<String>,
    pub cursor: Option<Value>,
    pub next_seq: u64,
    pub delivering: BTreeMap<String, u64>,
    pub seen: Vec<String>,
    pub outbox: Vec<PendingWrite>,
    pub posted_today: u64,
    pub post_day: u64,
    pub last_post_ms: Option<u64>,
    pub pause_shown: bool,
    /// Polls on which the chief could not receive a steering item, by item id.
    pub retries: BTreeMap<String, u32>,
}

impl Default for RemoteState {
    fn default() -> Self {
        Self {
            conversation: None,
            cursor: None,
            next_seq: 1,
            delivering: BTreeMap::new(),
            seen: Vec::new(),
            outbox: Vec::new(),
            posted_today: 0,
            post_day: 0,
            last_post_ms: None,
            pause_shown: false,
            retries: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PendingWrite {
    pub nonce: String,
    pub op: String,
    pub vars: Vars,
    pub attempts: u32,
    /// Saved before transport: a crash must not allow coalescing an observed event.
    pub attempted: bool,
    pub next_at: u64,
    /// Tool blocks give way to status, plan and approval writes when full.
    pub tool: bool,
    /// Only user/assistant/tool events may be coalesced or paused.
    pub mirror: bool,
    pub skipped_turns: u64,
    /// A refused replacement must never produce another replacement.
    pub withheld: bool,
}

impl PendingWrite {
    pub fn new(op: &str, nonce: String, vars: Vars) -> Self {
        Self { op: op.into(), nonce, vars, ..Self::default() }
    }

    pub fn retry(&mut self, now: u64) -> bool {
        self.attempts = self.attempts.saturating_add(1);
        self.next_at = now.saturating_add((2u64.saturating_pow(self.attempts)).min(300));
        self.attempts < 10
    }
}

impl RemoteState {
    pub fn reset_post_day(&mut self, now_ms: u64) {
        let day = now_ms / 86_400_000;
        if day != self.post_day {
            self.post_day = day;
            self.posted_today = 0;
            self.pause_shown = false;
        }
    }

    /// Drop tools first, then replace the oldest turns with one omission note.
    /// Status, approval, plan and acknowledgement writes are never coalesced.
    fn coalesce_mirror(&mut self, limit: usize) {
        while self.outbox.iter().filter(|w| w.mirror).count() > limit {
            if let Some(i) = self.outbox.iter().position(|w| w.mirror && w.tool && !w.attempted && w.attempts == 0) {
                self.outbox.remove(i);
                continue;
            }
            // Never change the content of an event that may already have reached the server.
            let Some(i) = self.outbox.iter().position(|w| w.mirror && !w.attempted && w.attempts == 0) else { break };
            let mut note = self.outbox.remove(i);
            let turn = note.vars.get("turn").cloned();
            let mut skipped = note.skipped_turns.max(1);
            self.outbox
                .retain(|w| !(w.mirror && !w.attempted && w.attempts == 0 && w.vars.get("turn") == turn.as_ref()));
            if note.skipped_turns > 0 {
                // Absorb the next oldest turn into the existing note.
                let Some(j) = self.outbox.iter().position(|w| w.mirror && !w.attempted && w.attempts == 0) else {
                    self.outbox.insert(i.min(self.outbox.len()), note);
                    break;
                };
                let next = self.outbox.remove(j);
                skipped += next.skipped_turns.max(1);
                let next_turn = next.vars.get("turn");
                self.outbox
                    .retain(|w| !(w.mirror && !w.attempted && w.attempts == 0 && w.vars.get("turn") == next_turn));
            }
            note.skipped_turns = skipped;
            note.vars = note.vars.with("body", Some(&format!("({skipped} turns skipped)")));
            self.outbox.insert(i.min(self.outbox.len()), note);
        }
    }

    pub fn remember(&mut self, id: String) {
        if !self.seen.contains(&id) {
            self.seen.push(id);
        }
        if self.seen.len() > MAX_SEEN {
            self.seen.drain(..self.seen.len() - MAX_SEEN);
        }
    }

    /// Returns a discarded tool nonce, if any. Only unattempted mirror events may be coalesced.
    pub fn enqueue(&mut self, write: PendingWrite) -> Result<Option<String>, String> {
        if !matches!(write.op.as_str(), "conversation_post" | "conversation_ack") {
            return Err("only conversation_post and conversation_ack may enter the remote outbox".into());
        }
        if write.nonce.is_empty() {
            return Err("an outbox write needs a nonce".into());
        }
        if self.outbox.iter().any(|pending| pending.nonce == write.nonce) {
            return Ok(None);
        }
        let dropped = if self.outbox.len() >= MAX_PENDING {
            if let Some(index) =
                self.outbox.iter().position(|pending| pending.tool && !pending.attempted && pending.attempts == 0)
            {
                Some(self.outbox.remove(index).nonce)
            } else {
                let mirrors = self.outbox.iter().filter(|w| w.mirror).count();
                self.coalesce_mirror(mirrors.saturating_sub(1));
                if self.outbox.len() >= MAX_PENDING {
                    return Err("remote outbox is full; the write was not queued".into());
                }
                None
            }
        } else {
            None
        };
        self.outbox.push(write);
        self.coalesce_mirror(MAX_MIRROR);
        Ok(dropped)
    }

    #[cfg(feature = "native")]
    pub fn path(home: &std::path::Path, plan: &str) -> anyhow::Result<std::path::PathBuf> {
        anyhow::ensure!(
            !plan.is_empty() && plan != "." && plan != ".." && !plan.contains(['/', '\\']),
            "invalid remote plan id"
        );
        Ok(home.join("plans").join(format!("{plan}.remote.json")))
    }

    /// A missing sidecar is the normal state of an older plan. Corruption is an error.
    #[cfg(feature = "native")]
    pub fn load(home: &std::path::Path, plan: &str) -> anyhow::Result<Self> {
        match std::fs::read(Self::path(home, plan)?) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }

    #[cfg(feature = "native")]
    pub fn save(&self, home: &std::path::Path, plan: &str) -> anyhow::Result<()> {
        use std::io::Write;
        let path = Self::path(home, plan)?;
        let dir = path.parent().expect("sidecar directory");
        std::fs::create_dir_all(dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.write_all(&serde_json::to_vec_pretty(self)?)?;
        tmp.as_file().sync_all()?;
        tmp.persist(&path)?;
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    }

    /// Reserve and persist a post sequence before its write is added to the outbox.
    #[cfg(feature = "native")]
    pub fn queue(
        &mut self,
        home: &std::path::Path,
        plan: &str,
        mut write: PendingWrite,
    ) -> anyhow::Result<Option<String>> {
        if self.outbox.iter().any(|pending| pending.nonce == write.nonce) {
            return Ok(None);
        }
        if write.op == "conversation_post" {
            let sequence = self.next_seq;
            self.next_seq = self.next_seq.checked_add(1).ok_or_else(|| anyhow::anyhow!("remote sequence exhausted"))?;
            self.save(home, plan)?;
            write.vars = write.vars.typed("seq", Value::from(sequence));
        }
        let dropped = self.enqueue(write).map_err(anyhow::Error::msg)?;
        self.save(home, plan)?;
        Ok(dropped)
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;
    use crate::plan::{Plan, load_plans};
    use serde_json::json;

    #[test]
    fn remote_sidecar_round_trip_old_plan_and_plan_json_are_separate() {
        let dir = crate::test_support::tempdir().unwrap();
        let home = dir.path();
        let plan = Plan { id: "old".into(), goal: "Older plan".into(), ..Default::default() };
        plan.save_in(home).unwrap();
        assert_eq!(RemoteState::load(home, "old").unwrap(), RemoteState::default());
        let mut state = RemoteState {
            conversation: Some("private".into()),
            cursor: Some(json!(42)),
            delivering: BTreeMap::from([("message".into(), 12)]),
            ..Default::default()
        };
        state.remember("seen".into());
        state.queue(home, "old", PendingWrite::new("conversation_post", "stable".into(), Vars::default())).unwrap();
        assert_eq!(state.next_seq, 2);
        assert_eq!(state.outbox[0].vars.get("seq"), Some(&json!(1)));
        assert_eq!(RemoteState::load(home, "old").unwrap(), state);
        let (plans, errors) = load_plans(home).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(plans.len(), 1);
        let value = serde_json::to_value(&plans[0]).unwrap();
        for key in ["conversation", "cursor", "outbox", "delivering", "seen", "next_seq", "posted_today"] {
            assert!(value.get(key).is_none(), "{key}");
        }
        assert_eq!(serde_json::from_value::<RemoteState>(json!({})).unwrap(), RemoteState::default());
        std::fs::write(RemoteState::path(home, "old").unwrap(), "broken").unwrap();
        assert!(RemoteState::load(home, "old").is_err());
        std::fs::remove_file(home.join("plans/old.json")).unwrap();
        assert_eq!(crate::plan::unique_id("old", &[], home), "old-2", "orphan sidecars reserve their plan id");
    }

    #[test]
    fn remote_mirror_pressure_preserves_attempted_nonces_and_protected_events() {
        let mut state = RemoteState::default();
        let mut observed = PendingWrite::new(
            "conversation_post",
            "observed".into(),
            Vars::default().with("turn", Some("old")).with("body", Some("unchanged")),
        );
        observed.mirror = true;
        observed.tool = true;
        observed.attempted = true;
        state.enqueue(observed.clone()).unwrap();
        for n in 0..100 {
            let mut write = PendingWrite::new(
                "conversation_post",
                n.to_string(),
                Vars::default().with("turn", Some(&n.to_string())),
            );
            write.mirror = true;
            state.enqueue(write).unwrap();
        }
        assert_eq!(state.outbox[0], observed);
        assert_eq!(state.outbox.len(), MAX_MIRROR);
        for n in 0..160 {
            state.enqueue(PendingWrite::new("conversation_ack", format!("ack-{n}"), Vars::default())).unwrap();
        }
        assert_eq!(state.outbox[0], observed);
        assert_eq!(state.outbox.iter().filter(|w| w.op == "conversation_ack").count(), 160);
        assert_eq!(state.outbox.len(), MAX_PENDING);
    }

    #[test]
    fn remote_bounds_backoff_and_tool_eviction() {
        let mut state = RemoteState::default();
        for i in 0..600 {
            state.remember(i.to_string());
        }
        assert_eq!(state.seen.len(), MAX_SEEN);
        assert_eq!(state.seen[0], "100");
        for i in 0..MAX_PENDING {
            let mut write = PendingWrite::new("conversation_ack", i.to_string(), Vars::default());
            write.tool = i == 10;
            state.enqueue(write).unwrap();
        }
        assert_eq!(
            state.enqueue(PendingWrite::new("conversation_ack", "new".into(), Vars::default())).unwrap(),
            Some("10".into())
        );
        assert_eq!(state.outbox.len(), MAX_PENDING);
        assert!(state.enqueue(PendingWrite::new("conversation_ack", "overflow".into(), Vars::default())).is_err());
        let mut write = PendingWrite::default();
        for attempt in 1..=10 {
            assert_eq!(write.retry(100), attempt < 10);
            assert_eq!(write.next_at, 100 + (1u64 << attempt).min(300));
        }
    }
}
