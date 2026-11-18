//! Bounded trace snapshots and pages, independent of the host and UI.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::plan::TaskStatus;
use crate::trace::{MAX_ENTRIES, MAX_ENTRY_BYTES, MAX_LOG_BYTES, TraceEntry};
use crate::wire::{ErrorCode, Seq, WireError};

pub const MAX_PAGE_ENTRIES: usize = 32;
pub const MAX_PAGE_BYTES: usize = 64 * 1024;
pub const MAX_STATES: usize = 25;

/// The current chief's cleaned trace. Published separately from board snapshots so a busy
/// trace cannot inflate the board's replay ring or hold up approvals.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TraceData {
    pub session: Option<String>,
    pub entries: Vec<TraceEntry>,
    /// Only state names actually present in the bounded trace, with their task status colors.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub states: BTreeMap<String, TaskStatus>,
}

impl TraceData {
    pub fn validate(&self) -> Result<(), WireError> {
        let bytes: usize = self.entries.iter().map(crate::wire::json_len).sum();
        if self.entries.len() > MAX_ENTRIES
            || bytes > MAX_LOG_BYTES
            || self.entries.iter().any(|e| crate::wire::json_len(e) > MAX_ENTRY_BYTES)
            || self.states.len() > MAX_STATES
            || self.states.keys().any(|s| s.len() > 300)
            || self.session.as_ref().is_some_and(|s| s.len() > crate::wire::MAX_ID_BYTES)
        {
            return Err(WireError::new(ErrorCode::Oversize, "trace exceeds its bounds"));
        }
        Ok(())
    }

    /// The trace as a client without `turns` is sent it: the calls alone.
    pub fn without_turns(&self) -> Self {
        Self {
            session: self.session.clone(),
            entries: self.entries.iter().filter(|entry| entry.turn.is_none()).cloned().collect(),
            states: self.states.clone(),
        }
    }

    /// Oldest first, pinned to a content revision. The caller reserves the envelope's space.
    pub fn page(&self, revision: u64, offset: u32, limit: u32, budget: usize) -> Result<TracePage, WireError> {
        self.validate()?;
        let start = offset as usize;
        if start > self.entries.len() {
            return Err(WireError::new(ErrorCode::Malformed, "trace offset is beyond the log"));
        }
        let mut page = TracePage {
            revision: Seq(revision),
            offset,
            data: Self { session: self.session.clone(), entries: Vec::new(), states: self.states.clone() },
            next_offset: None,
        };
        let end = (start + (limit as usize).clamp(1, MAX_PAGE_ENTRIES)).min(self.entries.len());
        let budget = budget.min(MAX_PAGE_BYTES);
        for entry in &self.entries[start..end] {
            page.data.entries.push(entry.clone());
            page.next_offset = Some(offset + page.data.entries.len() as u32);
            if crate::wire::json_len(&page) > budget {
                page.data.entries.pop();
                break;
            }
        }
        let next = start + page.data.entries.len();
        page.next_offset = (next < self.entries.len()).then_some(next as u32);
        if crate::wire::json_len(&page) > budget || (next == start && start < self.entries.len()) {
            return Err(WireError::new(ErrorCode::Oversize, "trace entry does not fit in a page"));
        }
        Ok(page)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TracePage {
    pub revision: Seq,
    pub offset: u32,
    pub data: TraceData,
    pub next_offset: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::HookEvent;
    use crate::tickets::Cleaner;
    use crate::trace::TraceLog;
    use crate::wire::{Envelope, Message};
    use serde_json::json;

    #[test]
    fn trace_host_and_wire_stay_bounded_and_pages_cover_the_retained_log() {
        let mut log = TraceLog::default();
        for n in 0..1000 {
            let event = HookEvent::from_payload(&json!({
                "session_id": "chief", "hook_event_name": "PostToolUse", "tool_name": "mcp__example__lookup",
                "tool_use_id": format!("call-{n}"),
                "tool_input": {"query": "🦀".repeat(5000)},
                "tool_response": {"text": "x".repeat(20000)}
            }))
            .unwrap();
            log.ingest(&event, None, &Cleaner::default(), n).unwrap();
            assert!(log.entries.len() <= MAX_ENTRIES);
            assert!(log.bytes() <= MAX_LOG_BYTES);
        }
        let data = TraceData {
            session: Some("chief".into()),
            entries: log.entries.into_iter().collect(),
            ..Default::default()
        };
        let mut offset = 0;
        let mut received = Vec::new();
        loop {
            // A smaller negotiated frame budget and an excessive requested count both apply.
            let page = data.page(7, offset, u32::MAX, 4096).unwrap();
            assert!(crate::wire::json_len(&page) <= 4096);
            assert!(page.data.entries.len() <= MAX_PAGE_ENTRIES);
            let encoded = crate::wire::encode(&Envelope::host("epoch", Message::TracePage(page.clone()))).unwrap();
            assert!(encoded.len() < 4608);
            assert_eq!(crate::wire::decode(&encoded).unwrap().message, Message::TracePage(page.clone()));
            received.extend(page.data.entries);
            match page.next_offset {
                Some(next) => {
                    assert!(next > offset);
                    offset = next;
                }
                None => break,
            }
        }
        assert_eq!(received, data.entries);
        assert_eq!(data.page(7, u32::MAX, 1, 4096).unwrap_err().code, ErrorCode::Malformed);
        assert_eq!(data.page(7, 0, 1, 1).unwrap_err().code, ErrorCode::Oversize);
        assert_eq!(data.page(7, 0, 0, MAX_PAGE_BYTES).unwrap().data.entries.len(), 1);

        let mut too_many = data.clone();
        too_many.entries = vec![data.entries[0].clone(); MAX_ENTRIES + 1];
        assert!(too_many.validate().is_err());
        let mut too_large = data;
        too_large.entries[0].summary = "x".repeat(MAX_ENTRY_BYTES);
        assert!(too_large.page(7, 0, 1, MAX_PAGE_BYTES).is_err());
    }
}
