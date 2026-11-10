//! A client's copy of the host's board, kept current by applying `atomics.v1` messages in order.
//!
//! Browser-safe and I/O free. Events carry consecutive cursors; a snapshot replaces everything,
//! and one sent in parts replaces it only once the last part is in. A gap (a missing event, or an
//! epoch that changed under a replay) is reported, never guessed over: the caller sends `resync`
//! and applies the snapshot that answers.

use std::collections::BTreeMap;

use crate::projection::{ApprovalInfo, DiffSummary, HostInfo, PlanInfo, SessionInfo};
use crate::wire::{Envelope, Message, Resume};

/// Why the mirror cannot follow the stream and needs a fresh snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gap {
    /// An event skipped from `have` to `got`.
    Missing { have: u64, got: u64 },
    /// The host's epoch is not the one this copy was built from.
    Epoch,
    /// An event arrived before the snapshot that a reset promised.
    NoSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    Changed,
    /// An event already applied (a replay overlapping a snapshot).
    Duplicate,
    /// One part of a snapshot sent in parts; the board changes when the last part arrives.
    Partial,
    /// A message that does not change the board.
    Ignored,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Mirror {
    pub epoch: Option<String>,
    pub cursor: u64,
    pub sessions: BTreeMap<String, SessionInfo>,
    pub plans: BTreeMap<String, PlanInfo>,
    pub approvals: BTreeMap<String, ApprovalInfo>,
    /// By worktree.
    pub diffs: BTreeMap<String, DiffSummary>,
    /// The host's own state: its workspace and the ticket sign-in.
    pub host: HostInfo,
    awaiting_snapshot: bool,
    /// The parts of a snapshot received so far. Only one is ever kept: a part of another
    /// snapshot starts over.
    assembly: Option<Assembly>,
}

#[derive(Debug, Clone, PartialEq)]
struct Assembly {
    epoch: Option<String>,
    cursor: u64,
    count: u16,
    parts: BTreeMap<u16, Board>,
}

/// One snapshot's (or one part's) collections.
#[derive(Debug, Clone, PartialEq)]
struct Board {
    sessions: Vec<SessionInfo>,
    plans: Vec<PlanInfo>,
    approvals: Vec<ApprovalInfo>,
    diffs: Vec<DiffSummary>,
    host: HostInfo,
}

impl Mirror {
    /// What to put in `hello` to resume: the epoch and cursor applied so far.
    pub fn resume_point(&self) -> (Option<String>, Option<u64>) {
        match (&self.epoch, self.awaiting_snapshot) {
            (Some(e), false) => (Some(e.clone()), Some(self.cursor)),
            _ => (None, None),
        }
    }

    pub fn apply(&mut self, envelope: &Envelope) -> Result<Applied, Gap> {
        if let Message::Welcome { resume, .. } = &envelope.message {
            match resume {
                Resume::Reset { .. } => {
                    *self = Mirror { epoch: envelope.epoch.clone(), awaiting_snapshot: true, ..Mirror::default() };
                }
                Resume::Replay { after } => {
                    if self.epoch != envelope.epoch || *after != crate::wire::Seq(self.cursor) {
                        return Err(Gap::Epoch);
                    }
                }
            }
            return Ok(Applied::Ignored);
        }
        if let (Some(have), Some(got)) = (&self.epoch, &envelope.epoch)
            && have != got
        {
            return Err(Gap::Epoch);
        }
        let cursor = match &envelope.message {
            Message::StateSnapshot { cursor, sessions, plans, approvals, diffs, host, part, .. } => {
                let board = Board {
                    sessions: sessions.clone(),
                    plans: plans.clone(),
                    approvals: approvals.clone(),
                    diffs: diffs.clone(),
                    host: host.clone(),
                };
                let parts = match part {
                    None => {
                        self.assembly = None;
                        vec![board]
                    }
                    Some(part) if part.index >= part.count => return Ok(Applied::Ignored),
                    Some(part) => {
                        let key = (&envelope.epoch, cursor.0, part.count);
                        if self.assembly.as_ref().is_none_or(|a| (&a.epoch, a.cursor, a.count) != key) {
                            self.assembly = Some(Assembly {
                                epoch: envelope.epoch.clone(),
                                cursor: cursor.0,
                                count: part.count,
                                parts: BTreeMap::new(),
                            });
                        }
                        let assembly = self.assembly.as_mut().expect("just set");
                        assembly.parts.insert(part.index, board);
                        if assembly.parts.len() < usize::from(assembly.count) {
                            return Ok(Applied::Partial);
                        }
                        self.assembly.take().expect("complete").parts.into_values().collect()
                    }
                };
                self.epoch = envelope.epoch.clone().or_else(|| self.epoch.take());
                self.cursor = cursor.0;
                self.awaiting_snapshot = false;
                self.sessions = parts.iter().flat_map(|b| &b.sessions).map(|s| (s.id.clone(), s.clone())).collect();
                self.plans = parts.iter().flat_map(|b| &b.plans).map(|p| (p.id.clone(), p.clone())).collect();
                self.approvals = parts.iter().flat_map(|b| &b.approvals).map(|a| (a.id.clone(), a.clone())).collect();
                self.diffs = parts.iter().flat_map(|b| &b.diffs).map(|d| (d.worktree.clone(), d.clone())).collect();
                self.host = parts.into_iter().next().map(|b| b.host).unwrap_or_default();
                return Ok(Applied::Changed);
            }
            Message::SessionUpsert { cursor, .. }
            | Message::SessionRemoved { cursor, .. }
            | Message::PlanUpsert { cursor, .. }
            | Message::PlanRemoved { cursor, .. }
            | Message::Skipped { cursor }
            | Message::TaskReport { cursor, .. }
            | Message::ApprovalPending { cursor, .. }
            | Message::ApprovalResolved { cursor, .. }
            | Message::DiffSummary { cursor, .. }
            | Message::HostUpsert { cursor, .. } => cursor.0,
            _ => return Ok(Applied::Ignored),
        };
        if self.awaiting_snapshot {
            return Err(Gap::NoSnapshot);
        }
        if cursor <= self.cursor {
            return Ok(Applied::Duplicate);
        }
        if cursor != self.cursor + 1 {
            return Err(Gap::Missing { have: self.cursor, got: cursor });
        }
        match &envelope.message {
            Message::SessionUpsert { session, .. } => {
                self.sessions.insert(session.id.clone(), session.clone());
            }
            Message::SessionRemoved { id, .. } => {
                self.sessions.remove(id);
            }
            Message::PlanUpsert { plan, .. } => {
                self.plans.insert(plan.id.clone(), plan.clone());
            }
            Message::PlanRemoved { id, .. } => {
                self.plans.remove(id);
            }
            Message::ApprovalPending { approval, .. } => {
                self.approvals.insert(approval.id.clone(), approval.clone());
            }
            Message::ApprovalResolved { id, .. } => {
                self.approvals.remove(id);
            }
            Message::DiffSummary { summary, .. } => {
                if summary.is_tombstone() {
                    self.diffs.remove(&summary.worktree);
                } else {
                    self.diffs.insert(summary.worktree.clone(), summary.clone());
                }
            }
            Message::HostUpsert { host, .. } => self.host = host.clone(),
            // A report is already inside the plan it belongs to, and a skipped event is one the
            // client asked not to see: both only advance the cursor.
            _ => {}
        }
        self.cursor = cursor;
        Ok(Applied::Changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::Projections;
    use crate::session::AgentStatus;
    use crate::wire::{Limits, ResetReason, Role, Seq, VERSION};

    fn session(id: &str, status: AgentStatus) -> SessionInfo {
        SessionInfo {
            status_hint: None,
            previews: Vec::new(),
            id: id.into(),
            agent: "claude".into(),
            title: id.into(),
            cwd: "/w".into(),
            worktree: None,
            branch: None,
            status,
            chief: false,
            task: None,
            pending_approvals: 0,
        }
    }

    fn upsert(cursor: u64, id: &str) -> Envelope {
        Envelope::host("e", Message::SessionUpsert { cursor: Seq(cursor), session: session(id, AgentStatus::Idle) })
    }

    fn snapshot() -> Envelope {
        let p = Projections::default();
        p.publish_sessions(vec![session("a", AgentStatus::Idle)]);
        Envelope::snapshot("e", &p.load())
    }

    fn welcome(resume: Resume) -> Envelope {
        Envelope::host(
            "e",
            Message::Welcome {
                version: VERSION,
                capabilities: vec![],
                limits: Limits::default(),
                role: Role::Viewer,
                resume,
                may: vec![],
            },
        )
    }

    #[test]
    fn events_apply_in_order_and_duplicates_are_ignored() {
        let mut m = Mirror::default();
        assert_eq!(m.apply(&snapshot()), Ok(Applied::Changed));
        assert_eq!(m.resume_point(), (Some("e".into()), Some(1)));
        assert_eq!(m.apply(&upsert(1, "a")), Ok(Applied::Duplicate));
        assert_eq!(m.apply(&upsert(2, "b")), Ok(Applied::Changed));
        assert_eq!(m.sessions.len(), 2);
        assert_eq!(m.apply(&upsert(4, "c")), Err(Gap::Missing { have: 2, got: 4 }));
        assert_eq!(m.sessions.len(), 2, "a gap changes nothing");
    }

    #[test]
    fn the_hosts_own_state_comes_with_the_snapshot_and_its_upserts() {
        let p = Projections::default();
        p.publish_sessions(vec![session("a", AgentStatus::Idle)]);
        p.publish_host(HostInfo { workspace: Some("team".into()), ..HostInfo::default() });
        let mut m = Mirror::default();
        m.apply(&Envelope::snapshot("e", &p.load())).unwrap();
        assert_eq!(m.host.workspace.as_deref(), Some("team"));
        let daily = HostInfo { workspace: Some("daily".into()), ..HostInfo::default() };
        let event = Envelope::host("e", Message::HostUpsert { cursor: Seq(3), host: daily.clone() });
        assert_eq!(m.apply(&event), Ok(Applied::Changed));
        assert_eq!((m.host.clone(), m.cursor), (daily, 3));
    }

    #[test]
    fn another_epoch_is_a_gap_and_a_reset_waits_for_its_snapshot() {
        let mut m = Mirror::default();
        m.apply(&snapshot()).unwrap();
        let mut other = upsert(2, "b");
        other.epoch = Some("other".into());
        assert_eq!(m.apply(&other), Err(Gap::Epoch));

        m.apply(&welcome(Resume::Reset { cursor: Seq(7), reason: ResetReason::CursorTooOld })).unwrap();
        assert_eq!(m.resume_point(), (None, None), "nothing to resume from until the snapshot lands");
        assert_eq!(m.apply(&upsert(8, "b")), Err(Gap::NoSnapshot));
        m.apply(&snapshot()).unwrap();
        assert_eq!(m.apply(&upsert(2, "b")), Ok(Applied::Changed));
    }

    /// `snapshot()`'s board with one more session, cut into `count` parts holding one session each.
    fn parts(cursor: u64) -> Vec<Envelope> {
        let sessions = [session("a", AgentStatus::Done), session("b", AgentStatus::Idle)];
        let count = sessions.len() as u16;
        sessions
            .iter()
            .enumerate()
            .map(|(index, s)| {
                let mut envelope = snapshot();
                let Message::StateSnapshot { cursor: c, sessions, part, .. } = &mut envelope.message else {
                    unreachable!()
                };
                *c = Seq(cursor);
                *sessions = vec![s.clone()];
                *part = Some(crate::wire::Part { index: index as u16, count });
                envelope
            })
            .collect()
    }

    #[test]
    fn a_snapshot_in_parts_changes_the_board_only_when_the_last_part_lands() {
        let mut m = Mirror::default();
        m.apply(&snapshot()).unwrap();
        let held = m.clone();
        let [first, last] = <[Envelope; 2]>::try_from(parts(9)).unwrap();
        assert_eq!(m.apply(&last), Ok(Applied::Partial), "parts may come in any order");
        assert_eq!((&m.sessions, m.cursor), (&held.sessions, held.cursor), "nothing shows before the last part");
        assert_eq!(m.apply(&first), Ok(Applied::Changed));
        assert_eq!(m.cursor, 9);
        assert_eq!(m.sessions.keys().collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(m.sessions["a"].status, AgentStatus::Done);

        // A part of a newer snapshot abandons a half-received one.
        let [stale, _] = <[Envelope; 2]>::try_from(parts(10)).unwrap();
        let [_, newer] = <[Envelope; 2]>::try_from(parts(11)).unwrap();
        assert_eq!(m.apply(&stale), Ok(Applied::Partial));
        assert_eq!(m.apply(&newer), Ok(Applied::Partial), "one part of 11 does not complete 10");
        assert_eq!(m.cursor, 9);
    }

    #[test]
    fn a_skipped_event_only_advances_the_cursor() {
        let mut m = Mirror::default();
        m.apply(&snapshot()).unwrap();
        let board = m.clone();
        assert_eq!(m.apply(&Envelope::host("e", Message::Skipped { cursor: Seq(2) })), Ok(Applied::Changed));
        assert_eq!((m.cursor, &m.sessions), (2, &board.sessions));
        assert_eq!(
            m.apply(&Envelope::host("e", Message::Skipped { cursor: Seq(4) })),
            Err(Gap::Missing { have: 2, got: 4 })
        );
        assert_eq!(m.apply(&upsert(3, "b")), Ok(Applied::Changed));
    }

    #[test]
    fn a_replay_must_continue_from_the_cursor_held() {
        let mut m = Mirror::default();
        m.apply(&snapshot()).unwrap();
        assert_eq!(m.apply(&welcome(Resume::Replay { after: Seq(1) })), Ok(Applied::Ignored));
        assert_eq!(m.apply(&welcome(Resume::Replay { after: Seq(5) })), Err(Gap::Epoch));
    }
}
