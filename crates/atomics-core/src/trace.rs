//! Chief MCP calls, reduced to cleaned display data before entering a bounded log.
//! Classification and ingestion are pure; only `disk` requires the native feature.

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::hook::HookEvent;
use crate::tickets::{self, Cleaner, TicketCard, TicketConfig};

pub const MAX_ENTRY_BYTES: usize = 16 * 1024;
pub const MAX_LOG_BYTES: usize = 512 * 1024;
pub const MAX_ENTRIES: usize = 200;
pub const MAX_CARDS: usize = 25;
pub const MAX_DISK_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceOp {
    Search,
    SemanticSearch,
    Read,
    Children,
    Related,
    Comment,
    LinkOrSync,
    Plan,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub server: String,
    pub tool: String,
    pub op: TraceOp,
    pub direct: bool,
    /// The profile operation used to parse a direct result.
    pub operation: Option<String>,
}

/// Match exact tool names, never server-specific substrings or result shapes.
pub fn classify(tool_name: &str, config: Option<&TicketConfig>) -> Option<Classification> {
    let (server, tool) = tool_name.strip_prefix("mcp__")?.split_once("__")?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    let mut classified =
        Classification { server: server.into(), tool: tool.into(), op: TraceOp::Other, direct: false, operation: None };
    if server == "atomics" {
        classified.op = match tool {
            "atomics_ticket_search" => TraceOp::Search,
            "atomics_ticket_read" => TraceOp::Read,
            "atomics_ticket_children" => TraceOp::Children,
            "atomics_ticket_related" => TraceOp::Related,
            "atomics_ticket_comment" => TraceOp::Comment,
            "atomics_ticket_link" | "atomics_ticket_sync" => TraceOp::LinkOrSync,
            "atomics_plan_set" | "atomics_plan_update" | "atomics_plan_stage" => TraceOp::Plan,
            _ => TraceOp::Other,
        };
    } else if let Some(config) = config {
        for (name, op) in [
            ("semantic_search", TraceOp::SemanticSearch),
            ("search", TraceOp::Search),
            ("read", TraceOp::Read),
            ("get", TraceOp::Read),
            ("children", TraceOp::Children),
            ("list", TraceOp::Children),
            ("related", TraceOp::Related),
            ("comment", TraceOp::Comment),
            ("update_state", TraceOp::LinkOrSync),
            ("create", TraceOp::LinkOrSync),
            ("record_stage", TraceOp::LinkOrSync),
        ] {
            if config.operation(name).is_some_and(|operation| operation.tool == tool) {
                classified.op = op;
                classified.direct = true;
                classified.operation = Some(name.into());
                break;
            }
        }
    }
    Some(classified)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TracePhase {
    Pre,
    Post,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceState {
    Running,
    Ok,
    Error,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceTask {
    pub id: String,
    pub title: String,
    pub depends_on: Vec<String>,
    pub ticket: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TracePlan {
    pub id: Option<String>,
    pub goal: String,
    pub tasks: Vec<TraceTask>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceEntry {
    pub seq: u64,
    /// Unix milliseconds, also allowing deterministic ingestion in tests and browser clients.
    pub at: u64,
    pub phase: TracePhase,
    pub tool_use_id: Option<String>,
    pub server: String,
    pub tool: String,
    pub op: TraceOp,
    pub direct: bool,
    #[serde(default)]
    pub plan_id: Option<String>,
    /// JSON argument summary, with no raw nested inputs.
    pub summary: String,
    pub state: TraceState,
    pub elapsed_ms: Option<u64>,
    pub cards: Vec<TicketCard>,
    pub plan: Option<TracePlan>,
    pub result_bytes: usize,
    pub error: Option<String>,
    pub hint: Option<String>,
    pub truncated: bool,
    pub omitted: usize,
}

/// A UTF-8 byte bound, including the ellipsis.
fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.into();
    }
    let mut end = max.saturating_sub(3);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn text(cleaner: &Cleaner, value: &str, chars: usize) -> String {
    cleaner.redacted(value).chars().take(chars).collect()
}

// Hook call ids often look exactly like tokens. Keep an opaque correlation id so
// token heuristics cannot collapse concurrent calls to the same `[redacted]` id.
fn call_id(id: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hasher);
    format!("call:{:016x}", hasher.finish())
}

fn summary(input: &Value, cleaner: &Cleaner) -> String {
    let summarize = |value: &Value| match value {
        Value::String(value) => json!(text(cleaner, value, 200)),
        Value::Array(items) => json!(format!("[{} items]", items.len())),
        Value::Object(items) => json!(format!("[{} keys]", items.len())),
        other => other.clone(),
    };
    let value = match input.as_object() {
        Some(map) => Value::Object(
            map.iter()
                .take(100)
                .map(|(key, value)| {
                    (
                        text(cleaner, key, 200),
                        if tickets::secret_name(key) { json!("[redacted]") } else { summarize(value) },
                    )
                })
                .collect(),
        ),
        None => summarize(input),
    };
    cut(&value.to_string(), 1024)
}

/// MCP hooks have shipped content arrays, result objects and strings. Normalize once.
fn output(value: &Value) -> Value {
    match value {
        Value::Array(_) => tickets::tool_output(&json!({"content": value})),
        Value::String(s) => serde_json::from_str(s).map(|value| output(&value)).unwrap_or_else(|_| value.clone()),
        Value::Object(map) if map.contains_key("structuredContent") || map.contains_key("content") => {
            tickets::tool_output(value)
        }
        _ => value.clone(),
    }
}

fn response_error(response: &Value, parsed: &Value) -> Option<Value> {
    if let Some(serialized) = response.as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()) {
        return response_error(&serialized, parsed);
    }
    if let Some(error) = response.get("error").filter(|error| !error.is_null()) {
        return Some(error.clone());
    }
    if response.get("isError").and_then(Value::as_bool) == Some(true) {
        return Some(parsed.clone());
    }
    parsed.get("error").filter(|error| !error.is_null()).cloned()
}

fn error_text(value: Value, cleaner: &Cleaner) -> String {
    fn scrub(value: Value, cleaner: &Cleaner) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .map(|(key, value)| {
                        let value =
                            if tickets::secret_name(&key) { json!("[redacted]") } else { scrub(value, cleaner) };
                        (cleaner.redacted(&key), value)
                    })
                    .collect(),
            ),
            Value::Array(items) => Value::Array(items.into_iter().map(|value| scrub(value, cleaner)).collect()),
            Value::String(s) => json!(cleaner.redacted(&s)),
            other => other,
        }
    }
    let value = match value {
        Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
        other => other,
    };
    let cleaned = scrub(value, cleaner);
    let message = cleaned.as_str().map(str::to_string).unwrap_or_else(|| cleaned.to_string());
    text(cleaner, &message, 300)
}

fn plan(input: &Value, cleaner: &Cleaner) -> TracePlan {
    let field = |value: &Value| value.as_str().map(|s| text(cleaner, s, 200));
    TracePlan {
        id: input.get("plan").and_then(field),
        goal: input.get("goal").and_then(field).unwrap_or_default(),
        tasks: input
            .get("tasks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .take(MAX_CARDS)
            .map(|task| TraceTask {
                id: task.get("id").and_then(field).unwrap_or_default(),
                title: task.get("title").and_then(field).unwrap_or_default(),
                depends_on: task
                    .get("depends_on")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .take(MAX_CARDS)
                    .filter_map(field)
                    .collect(),
                ticket: task
                    .get("ticket")
                    .and_then(|ticket| {
                        ticket.as_str().map(str::to_string).or_else(|| {
                            ticket.get("key").or_else(|| ticket.get("id")).and_then(Value::as_str).map(str::to_string)
                        })
                    })
                    .map(|s| text(cleaner, &s, 200)),
            })
            .collect(),
    }
}

/// Clean every string, including link fields and property names, before a card can enter the log.
fn clean_value(value: Value, cleaner: &Cleaner) -> Value {
    match value {
        Value::String(s) => json!(cleaner.redacted(&s)),
        Value::Array(items) => Value::Array(items.into_iter().map(|v| clean_value(v, cleaner)).collect()),
        Value::Object(map) => {
            Value::Object(map.into_iter().map(|(k, v)| (cleaner.redacted(&k), clean_value(v, cleaner))).collect())
        }
        other => other,
    }
}

fn bounded(mut entry: TraceEntry) -> TraceEntry {
    entry.summary = cut(&entry.summary, 1024);
    entry.server = entry.server.chars().take(200).collect();
    entry.tool = entry.tool.chars().take(200).collect();
    entry.tool_use_id = entry.tool_use_id.map(|id| id.chars().take(200).collect());
    entry.error = entry.error.map(|error| error.chars().take(300).collect());
    entry.hint = entry.hint.map(|hint| hint.chars().take(300).collect());
    entry.plan_id = entry.plan_id.map(|id| id.chars().take(128).collect());
    if let Some(plan) = &mut entry.plan {
        plan.id = plan.id.take().map(|id| id.chars().take(200).collect());
        plan.goal = plan.goal.chars().take(200).collect();
    }
    fn prune(cards: &mut Vec<TicketCard>, left: &mut usize, omitted: &mut usize) {
        let count = cards.len().min(*left);
        *omitted += cards.len() - count;
        cards.truncate(count);
        *left -= count;
        for card in cards {
            prune(&mut card.children, left, omitted);
            prune(&mut card.related, left, omitted);
        }
    }
    let mut remaining = MAX_CARDS;
    prune(&mut entry.cards, &mut remaining, &mut entry.omitted);
    while serde_json::to_vec(&entry).expect("trace serializes").len() > MAX_ENTRY_BYTES {
        if entry.cards.pop().is_some() || entry.plan.as_mut().is_some_and(|plan| plan.tasks.pop().is_some()) {
            entry.omitted += 1;
        } else {
            // All other fields are individually bounded; persisted input is re-bounded on load.
            entry.summary = cut(&entry.summary, 1024);
            entry.hint = entry.hint.map(|s| cut(&s, 1024));
            break;
        }
    }
    entry
}

/// Ingest only reduced, sanitized entries. Session authorization belongs to the host.
pub fn ingest(event: &HookEvent, config: Option<&TicketConfig>, cleaner: &Cleaner, at: u64) -> Option<TraceEntry> {
    let phase = match event.name.as_str() {
        "PreToolUse" => TracePhase::Pre,
        "PostToolUse" => TracePhase::Post,
        _ => return None,
    };
    let mut classification = classify(event.tool_name.as_deref()?, config)?;
    if classification.server == "atomics"
        && classification.tool == "atomics_ticket_search"
        && (event.tool_input.get("semantic").and_then(Value::as_bool) == Some(true)
            || event.tool_input.get("mode").and_then(Value::as_str) == Some("semantic"))
    {
        classification.op = TraceOp::SemanticSearch;
    }
    let mut entry = TraceEntry {
        seq: 0,
        at,
        phase,
        tool_use_id: event.tool_use_id.as_deref().map(call_id),
        server: text(cleaner, &classification.server, 200),
        tool: text(cleaner, &classification.tool, 200),
        op: classification.op,
        direct: classification.direct,
        plan_id: None,
        summary: summary(&event.tool_input, cleaner),
        state: if phase == TracePhase::Pre { TraceState::Running } else { TraceState::Ok },
        elapsed_ms: None,
        cards: Vec::new(),
        plan: None,
        result_bytes: if phase == TracePhase::Post {
            event
                .tool_response
                .get("original_bytes")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or_else(|| event.tool_response.to_string().len())
        } else {
            0
        },
        error: None,
        hint: None,
        truncated: event.tool_response.get("truncated") == Some(&Value::Bool(true)),
        omitted: 0,
    };
    if entry.op == TraceOp::Plan {
        let plan = plan(&event.tool_input, cleaner);
        entry.omitted = event
            .tool_input
            .get("tasks")
            .and_then(Value::as_array)
            .map_or(0, |tasks| tasks.len().saturating_sub(MAX_CARDS));
        entry.plan = Some(plan);
    }
    if phase == TracePhase::Post {
        let parsed = output(&event.tool_response);
        if let Some(error) = response_error(&event.tool_response, &parsed) {
            entry.state = TraceState::Error;
            entry.error = Some(error_text(error, cleaner));
        } else if !matches!(entry.op, TraceOp::Other | TraceOp::Plan | TraceOp::Comment) && !entry.truncated {
            let cards = if let Some(name) = classification.operation.as_deref() {
                let config = config.expect("direct operation has a profile");
                let operation = config.operation(name).expect("classified operation");
                if matches!(entry.op, TraceOp::Search | TraceOp::SemanticSearch | TraceOp::Children | TraceOp::Related)
                {
                    // Limit before mapping, not after allocating a whole server's ticket list.
                    let mut limited = parsed.clone();
                    if let Some(Value::Array(items)) = path_mut(&mut limited, &operation.items) {
                        entry.omitted = items.len().saturating_sub(MAX_CARDS);
                        items.truncate(MAX_CARDS);
                    }
                    operation.cards(&limited, &config.system)
                } else {
                    operation.card(&parsed, &config.system).map(|card| vec![card])
                }
                .and_then(|cards| cards.into_iter().map(|card| config.card_url(card)).collect())
            } else {
                entry.omitted = parsed
                    .get("cards")
                    .and_then(Value::as_array)
                    .map_or(0, |cards| cards.len().saturating_sub(MAX_CARDS));
                tickets::reply_cards(&parsed)
            };
            match cards {
                Ok(cards) => entry.cards = cards.into_iter().take(MAX_CARDS).map(|card| cleaner.card(card)).collect(),
                Err(_) => {
                    let operation = classification.operation.as_deref().unwrap_or("ticket card reply");
                    let sample = error_text(parsed, cleaner);
                    entry.hint = Some(text(cleaner, &format!("result paths do not match {operation}: {sample}"), 300));
                }
            }
        }
    }
    Some(bounded(entry))
}

fn path_mut<'a>(value: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    let mut current = value;
    let path = path.trim().trim_start_matches('$').replace('[', ".").replace(']', "");
    for key in path.split('.').filter(|key| !key.is_empty()) {
        current = current.get_mut(key)?;
    }
    Some(current)
}

#[derive(Debug, Default)]
pub struct TraceLog {
    pub entries: VecDeque<TraceEntry>,
    next_seq: u64,
    bytes: usize,
}

impl TraceLog {
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn recent(&self, limit: usize) -> Vec<&TraceEntry> {
        self.entries.iter().skip(self.entries.len().saturating_sub(limit.min(MAX_ENTRIES))).collect()
    }

    pub fn ingest(
        &mut self,
        event: &HookEvent,
        config: Option<&TicketConfig>,
        cleaner: &Cleaner,
        at: u64,
    ) -> Option<&TraceEntry> {
        self.ingest_for_plan(event, config, cleaner, at, None)
    }

    pub fn ingest_for_plan(
        &mut self,
        event: &HookEvent,
        config: Option<&TicketConfig>,
        cleaner: &Cleaner,
        at: u64,
        plan: Option<&str>,
    ) -> Option<&TraceEntry> {
        let mut entry = ingest(event, config, cleaner, at)?;
        entry.plan_id = plan.map(|id| cleaner.text(id));
        if let Some(previous) = self.entries.iter().find(|old| {
            entry.tool_use_id.is_some()
                && old.tool_use_id == entry.tool_use_id
                && old.server == entry.server
                && old.tool == entry.tool
                && old.state == TraceState::Running
        }) {
            entry.seq = previous.seq;
            entry.plan_id = previous.plan_id.clone();
            if entry.phase == TracePhase::Post {
                entry.elapsed_ms = Some(at.saturating_sub(previous.at));
                if event.tool_input.is_null() {
                    entry.summary = previous.summary.clone();
                    entry.op = previous.op;
                }
                if entry.plan.as_ref().is_some_and(|plan| plan.tasks.is_empty()) {
                    entry.plan = previous.plan.clone();
                    entry.omitted = previous.omitted;
                }
            }
        } else {
            self.next_seq += 1;
            entry.seq = self.next_seq;
        }
        let seq = entry.seq;
        self.insert(bounded(entry));
        self.entries.iter().find(|entry| entry.seq == seq)
    }

    pub fn merge(&mut self, other: TraceLog) {
        for entry in other.entries {
            if !self.entries.iter().any(|old| old.seq == entry.seq && old.at > entry.at) {
                self.insert(entry);
            }
        }
    }

    pub fn remove_plan(&mut self, plan: &str) {
        self.entries.retain(|entry| entry.plan_id.as_deref() != Some(plan));
        self.bytes = self.entries.iter().map(|entry| serde_json::to_vec(entry).expect("trace serializes").len()).sum();
    }

    fn insert(&mut self, entry: TraceEntry) {
        self.next_seq = self.next_seq.max(entry.seq);
        if let Some(index) = self.entries.iter().position(|old| old.seq == entry.seq) {
            let old = self.entries.remove(index).expect("existing entry");
            self.bytes -= serde_json::to_vec(&old).expect("trace serializes").len();
        }
        let index = self.entries.iter().position(|old| old.seq > entry.seq).unwrap_or(self.entries.len());
        self.bytes += serde_json::to_vec(&entry).expect("trace serializes").len();
        self.entries.insert(index, entry);
        while self.entries.len() > MAX_ENTRIES || self.bytes > MAX_LOG_BYTES {
            if let Some(old) = self.entries.pop_front() {
                self.bytes -= serde_json::to_vec(&old).expect("trace serializes").len();
            }
        }
    }
}

#[cfg(feature = "native")]
pub mod disk {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::io::{self, Read, Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};

    pub fn path(home: &Path, plan: Option<&str>) -> io::Result<PathBuf> {
        let name = plan.unwrap_or("chief");
        if name.is_empty() || name.len() > 128 || !name.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid trace name"));
        }
        Ok(home.join("trace").join(format!("{name}.jsonl")))
    }

    fn open(path: &Path) -> io::Result<std::fs::File> {
        let parent = path.parent().ok_or_else(|| io::Error::other("trace directory missing"))?;
        fs::create_dir_all(parent)?;
        let mut options = OpenOptions::new();
        options.create(true).read(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            // create_dir_all above also supports Windows; restrict an existing directory and file.
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        Ok(file)
    }

    /// Append the very same sanitized entry as the memory log. Updates share their Pre's sequence.
    pub fn append(path: &Path, entry: &TraceEntry) -> io::Result<()> {
        let mut line = serde_json::to_vec(entry)?;
        if line.len() > MAX_ENTRY_BYTES {
            return Err(io::Error::other("trace entry too large"));
        }
        line.push(b'\n');
        let mut file = open(path)?;
        // A crash may leave an incomplete last write. Repair only that tail before
        // appending, otherwise the next valid record would join the broken line.
        let len = file.metadata()?.len();
        if len > 0 {
            file.seek(SeekFrom::End(-1))?;
            let mut last = [0];
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                let start = len.saturating_sub((MAX_ENTRY_BYTES + 1) as u64);
                file.seek(SeekFrom::Start(start))?;
                let mut tail = Vec::new();
                (&mut file).take((MAX_ENTRY_BYTES + 1) as u64).read_to_end(&mut tail)?;
                let complete = tail.iter().rposition(|byte| *byte == b'\n').map_or(0, |at| start + at as u64 + 1);
                file.set_len(complete)?;
            }
        }
        if file.metadata()?.len() + line.len() as u64 > MAX_DISK_BYTES as u64 {
            let len = file.metadata()?.len();
            file.seek(SeekFrom::Start(len.saturating_sub((MAX_DISK_BYTES / 2) as u64)))?;
            let mut tail = Vec::new();
            (&mut file).take((MAX_DISK_BYTES / 2) as u64).read_to_end(&mut tail)?;
            // Start after a complete line even when seeking into a multibyte character.
            let start = tail.iter().position(|b| *b == b'\n').map_or(tail.len(), |at| at + 1);
            let mut replacement = tempfile::NamedTempFile::new_in(path.parent().expect("parent"))?;
            replacement.write_all(&tail[start..])?;
            replacement.write_all(&line)?;
            replacement.flush()?;
            drop(file);
            replacement.persist(path).map_err(|error| error.error)?;
        } else {
            file.write_all(&line)?;
        }
        Ok(())
    }

    pub fn load(path: &Path, cleaner: &Cleaner) -> io::Result<TraceLog> {
        let mut log = TraceLog::default();
        if !path.exists() {
            return Ok(log);
        }
        let mut file = open(path)?;
        let len = file.metadata()?.len();
        let offset = len.saturating_sub(MAX_DISK_BYTES as u64);
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        file.take(MAX_DISK_BYTES as u64).read_to_end(&mut bytes)?;
        for (i, line) in bytes.split(|b| *b == b'\n').enumerate() {
            if (i == 0 && offset > 0) || line.len() > MAX_ENTRY_BYTES {
                continue;
            }
            if let Ok(entry) = serde_json::from_slice::<TraceEntry>(line) {
                // Internal plan ids are validated filenames, rather than server text.
                let plan_id = entry.plan_id.as_ref().map(|id| cleaner.text(id));
                let cards: Vec<_> = entry.cards.iter().cloned().map(|card| cleaner.card(card)).collect();
                let value = clean_value(serde_json::to_value(TraceEntry { cards: Vec::new(), ..entry })?, cleaner);
                if let Ok(mut entry) = serde_json::from_value::<TraceEntry>(value) {
                    entry.plan_id = plan_id;
                    entry.cards = cards;
                    log.insert(bounded(entry));
                }
            }
        }
        Ok(log)
    }

    pub fn remove(home: &Path, plan: &str) -> io::Result<()> {
        match fs::remove_file(path(home, Some(plan))?) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tickets::{FieldPaths, Operation, TicketLink};

    fn event(name: &str, id: &str, tool: &str, input: Value, response: Value) -> HookEvent {
        HookEvent::from_payload(&json!({"hook_event_name": name, "session_id": "chief-session",
            "tool_name": tool, "tool_use_id": id, "tool_input": input, "tool_response": response}))
        .unwrap()
    }

    fn card(id: usize) -> TicketCard {
        TicketCard {
            link: TicketLink {
                id: id.to_string(),
                key: Some(format!("DEMO-{id}")),
                title: Some("A ticket".into()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn config() -> TicketConfig {
        let mut config = TicketConfig { system: "example".into(), ..Default::default() };
        config.operations.search = Some(Operation {
            tool: "find".into(),
            items: "$.result.rows".into(),
            result: FieldPaths { id: "id".into(), title: "subject".into(), ..Default::default() },
            ..Default::default()
        });
        config
    }

    #[test]
    fn trace_classification_is_exact_and_profile_driven() {
        for (tool, op) in [
            ("atomics_ticket_search", TraceOp::Search),
            ("atomics_ticket_read", TraceOp::Read),
            ("atomics_ticket_children", TraceOp::Children),
            ("atomics_ticket_related", TraceOp::Related),
            ("atomics_ticket_comment", TraceOp::Comment),
            ("atomics_ticket_link", TraceOp::LinkOrSync),
            ("atomics_ticket_sync", TraceOp::LinkOrSync),
            ("atomics_plan_set", TraceOp::Plan),
            ("atomics_plan_update", TraceOp::Plan),
            ("atomics_plan_stage", TraceOp::Plan),
        ] {
            let found = classify(&format!("mcp__atomics__{tool}"), None).unwrap();
            assert_eq!(found.op, op);
            assert!(!found.direct);
        }
        let config = config();
        assert!(classify("Bash", Some(&config)).is_none());
        assert!(classify("mcp____find", Some(&config)).is_none());
        let found = classify("mcp__any_server__find", Some(&config)).unwrap();
        assert_eq!(found.op, TraceOp::Search);
        assert!(found.direct);
        assert_eq!(classify("mcp__any__find_extra", Some(&config)).unwrap().op, TraceOp::Other);
        assert_eq!(classify("mcp__untrusted__atomics_ticket_read", None).unwrap().op, TraceOp::Other);
    }

    #[test]
    fn trace_pairs_concurrent_ids_and_keeps_plan_at_pre_time() {
        let cleaner = Cleaner::default();
        let mut log = TraceLog::default();
        let input = json!({"goal":"Build", "tasks":[{"id":"a", "title":"One"},
            {"id":"b", "title":"Two", "depends_on":["a"], "ticket":{"id":"DEMO-2"}}]});
        let first = event(
            "PreToolUse",
            "toolu_concurrent123456789012345_a",
            "mcp__atomics__atomics_plan_set",
            input,
            Value::Null,
        );
        let second = event(
            "PreToolUse",
            "toolu_concurrent123456789012345_b",
            "mcp__atomics__atomics_ticket_read",
            json!({}),
            Value::Null,
        );
        let running = log.ingest_for_plan(&first, None, &cleaner, 1000, Some("plan-a")).unwrap();
        assert_eq!(running.plan.as_ref().unwrap().tasks[1].depends_on, ["a"]);
        assert_eq!(running.state, TraceState::Running);
        log.ingest(&second, None, &cleaner, 1100).unwrap();
        let post = HookEvent { name: "PostToolUse".into(), tool_input: Value::Null, ..first.clone() };
        let done = log.ingest_for_plan(&post, None, &cleaner, 1450, Some("plan-b")).unwrap();
        assert_eq!(done.seq, 1);
        assert_eq!(done.elapsed_ms, Some(450));
        assert_eq!(done.plan_id.as_deref(), Some("plan-a"));
        assert_eq!(done.plan.as_ref().unwrap().tasks.len(), 2);
        assert_eq!(log.entries.len(), 2);
        assert_eq!(log.entries[1].state, TraceState::Running);
    }

    #[test]
    fn trace_post_without_pre_maps_cards_and_unmapped_results_are_discarded() {
        let mut log = TraceLog::default();
        let response = json!({"structuredContent": tickets::card_reply(&card(1))});
        let post = event("PostToolUse", "read", "mcp__atomics__atomics_ticket_read", json!({}), response);
        let entry = log.ingest(&post, None, &Cleaner::default(), 10).unwrap();
        assert_eq!(entry.cards[0].link.id, "1");
        assert_eq!(entry.elapsed_ms, None);
        let post = event(
            "PostToolUse",
            "other",
            "mcp__other__lookup",
            json!({}),
            json!({"private":"raw-result-must-never-be-saved"}),
        );
        let entry = log.ingest(&post, None, &Cleaner::default(), 20).unwrap();
        assert!(entry.cards.is_empty());
        assert!(entry.result_bytes > 0);
        assert!(!serde_json::to_string(entry).unwrap().contains("raw-result-must-never-be-saved"));
    }

    #[test]
    fn trace_direct_mapping_caps_before_parsing_and_reports_mismatch() {
        let config = config();
        let response =
            json!({"result":{"rows": (0..100).map(|i| json!({"id":i.to_string(),"subject":"A"})).collect::<Vec<_>>()}});
        let post = event("PostToolUse", "direct", "mcp__profile__find", json!({}), response);
        let entry = ingest(&post, Some(&config), &Cleaner::default(), 0).unwrap();
        assert_eq!(entry.cards.len(), MAX_CARDS);
        assert_eq!(entry.omitted, 75);
        assert!(entry.direct);
        let post = HookEvent { tool_response: json!({"wrong":true}), ..post };
        let entry = ingest(&post, Some(&config), &Cleaner::default(), 0).unwrap();
        assert!(entry.cards.is_empty());
        assert!(entry.hint.as_ref().unwrap().contains("result paths do not match"));
    }

    #[test]
    fn trace_ring_keeps_two_hundred_and_enforces_byte_and_entry_caps() {
        let mut log = TraceLog::default();
        let mut post = event(
            "PostToolUse",
            "call",
            "mcp__atomics__atomics_ticket_search",
            json!({}),
            tickets::cards_reply(&[card(1)]),
        );
        for i in 0..300 {
            log.ingest(&post, None, &Cleaner::default(), i).unwrap();
        }
        assert_eq!(log.entries.len(), 200);
        assert_eq!(log.entries[0].seq, 101);
        let mut big = card(1);
        big.body = Some("word ".repeat(10000));
        post.tool_response = tickets::cards_reply(&[big]);
        let entry = log.ingest(&post, None, &Cleaner::default(), 301).unwrap();
        assert!(serde_json::to_vec(entry).unwrap().len() <= MAX_ENTRY_BYTES);
        assert_eq!(entry.omitted, 1);
        assert!(entry.cards.is_empty());
        let mut medium = card(2);
        medium.body = Some("word ".repeat(1600));
        post.tool_response = tickets::cards_reply(&[medium]);
        for i in 0..100 {
            log.ingest(&post, None, &Cleaner::default(), 400 + i).unwrap();
        }
        assert!(log.entries.len() < 200);
        assert!(log.bytes() <= MAX_LOG_BYTES);
    }

    #[test]
    fn trace_argument_summaries_are_bounded_and_do_not_keep_nested_values() {
        let post = event(
            "PostToolUse",
            "args",
            "mcp__other__find",
            json!({
                "q":"é".repeat(1000), "api_key":"short", "cookie":"bite", "items":["nested-private"], "object":{"password":"nested-private"}
            }),
            Value::Null,
        );
        let entry = ingest(&post, None, &Cleaner::default(), 0).unwrap();
        assert!(entry.summary.len() <= 1024);
        assert!(!entry.summary.contains("nested-private"));
        assert!(!entry.summary.contains("short"));
        assert!(!entry.summary.contains("bite"));
        assert!(entry.summary.contains("[1 items]"));
        assert!(entry.summary.contains("[1 keys]"));
    }

    #[test]
    fn trace_real_payload_shapes_and_recorded_codex_fields() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/example-mcp-trace-hooks.json")).unwrap();
        let config: TicketConfig = serde_json::from_value(fixture["profile"].clone()).unwrap();
        for payload in fixture["events"].as_array().unwrap() {
            let event = HookEvent::from_payload(payload).unwrap();
            let entry = ingest(&event, Some(&config), &Cleaner::default(), 0).unwrap();
            if payload["tool_response"]["isError"] == true {
                assert_eq!(entry.state, TraceState::Error);
                assert!(entry.error.as_ref().unwrap().contains("unauthorized"));
            } else {
                assert_eq!(entry.cards[0].link.key.as_deref(), fixture["expected_key"].as_str());
                assert_eq!(entry.cards[0].link.id, fixture["expected_id"].as_str().unwrap());
            }
        }
        let recorded: Value = serde_json::from_str(include_str!("../tests/fixtures/codex-0.165-hooks.json")).unwrap();
        let event = HookEvent::from_payload(&recorded["events"][1]).unwrap();
        assert!(event.tool_use_id.as_ref().unwrap().starts_with("exec-"));
        assert_eq!(event.tool_response, json!(""));
        assert!(ingest(&event, None, &Cleaner::default(), 0).is_none());
    }

    #[cfg(feature = "native")]
    #[test]
    fn trace_secrets_never_reach_memory_or_jsonl_in_any_display_field() {
        let dir = crate::test_support::tempdir().unwrap();
        let path = disk::path(dir.path(), Some("plan-a")).unwrap();
        let secret = "configured-secret-37";
        let cleaner = Cleaner::new([secret.into(), "refresh-secret-38".into(), "client-secret-39".into()]);
        let mut secret_card = card(1);
        secret_card.link.title = Some(format!("{secret} Bearer bare-credential"));
        secret_card.link.url = Some("https://user:pass@host/ticket?api_key=url-secret&safe=1".into());
        secret_card.link.state = Some("Basic basic-credential".into());
        secret_card.body = Some(format!("{secret} refresh-secret-38 client-secret-39"));
        secret_card.snippet =
            Some("https://user:pass@host/?code=auth-code&api%5fkey=encoded-query#cookie=fragment-cookie".into());
        secret_card.comments.push(tickets::Comment { text: secret.into(), ..Default::default() });
        secret_card.fields.insert(secret.into(), secret.into());
        secret_card.fields.insert("api_token".into(), "field-credential".into());
        secret_card.kind = Some("Abc123456789012345678901234567890".into());
        let mut post = event(
            "PostToolUse",
            secret,
            "mcp__atomics__atomics_ticket_read",
            json!({
                (secret): secret, "token": "unknown-secret", "cookie":"session-cookie", "query": format!("https://user:pass@host/?token=url-secret {secret}"),
                "nested": {"private":"not-retained"}
            }),
            json!({"structuredContent": tickets::card_reply(&secret_card)}),
        );
        let mut log = TraceLog::default();
        let entry = log.ingest(&post, None, &cleaner, 1).unwrap();
        disk::append(&path, entry).unwrap();
        post.tool_response = json!({"isError": true, "content":[{"type":"text", "text": format!("{secret} Authorization: Bearer error-credential https://u:p@host/?password=error-password")}]});
        disk::append(&path, log.ingest(&post, None, &cleaner, 2).unwrap()).unwrap();
        post.name = "PreToolUse".into();
        post.tool_name = Some("mcp__atomics__atomics_plan_set".into());
        post.tool_input =
            json!({"goal":secret, "tasks":[{"id":secret,"title":secret,"depends_on":[secret],"ticket":{"id":secret}}]});
        disk::append(&path, log.ingest(&post, None, &cleaner, 3).unwrap()).unwrap();
        let memory = serde_json::to_string(&log.entries).unwrap();
        let disk = std::fs::read_to_string(&path).unwrap();
        for forbidden in [
            secret,
            "refresh-secret-38",
            "client-secret-39",
            "bare-credential",
            "basic-credential",
            "user:pass",
            "url-secret",
            "unknown-secret",
            "session-cookie",
            "not-retained",
            "field-credential",
            "auth-code",
            "encoded-query",
            "fragment-cookie",
            "Abc123456789012345678901234567890",
            "error-credential",
            "u:p@",
            "error-password",
        ] {
            assert!(!memory.contains(forbidden), "memory leaked {forbidden}");
            assert!(!disk.contains(forbidden), "disk leaked {forbidden}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[cfg(feature = "native")]
    #[test]
    fn trace_jsonl_rotates_reloads_updates_and_ignores_partial_lines() {
        let dir = crate::test_support::tempdir().unwrap();
        let path = disk::path(dir.path(), Some("plan-a")).unwrap();
        let mut log = TraceLog::default();
        let mut c = card(1);
        c.body = Some("body ".repeat(1000));
        let mut post =
            event("PostToolUse", "same", "mcp__atomics__atomics_ticket_read", json!({}), tickets::card_reply(&c));
        for i in 0..240 {
            disk::append(&path, log.ingest(&post, None, &Cleaner::default(), i).unwrap()).unwrap();
        }
        assert!(std::fs::metadata(&path).unwrap().len() < MAX_DISK_BYTES as u64);
        assert!(std::fs::read_to_string(&path).unwrap().lines().count() < 200);
        post.name = "PreToolUse".into();
        disk::append(&path, log.ingest(&post, None, &Cleaner::default(), 1000).unwrap()).unwrap();
        post.name = "PostToolUse".into();
        disk::append(&path, log.ingest(&post, None, &Cleaner::default(), 1200).unwrap()).unwrap();
        use std::io::Write;
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{broken partial").unwrap();
        let loaded = disk::load(&path, &Cleaner::default()).unwrap();
        assert_eq!(loaded.entries.back().unwrap(), log.entries.back().unwrap());
        assert_eq!(loaded.entries.back().unwrap().elapsed_ms, Some(200));
        assert!(loaded.bytes() <= MAX_LOG_BYTES);
        disk::append(&path, log.ingest(&post, None, &Cleaner::default(), 1300).unwrap()).unwrap();
        let repaired = disk::load(&path, &Cleaner::default()).unwrap();
        assert_eq!(repaired.entries.back().unwrap().at, 1300);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        disk::remove(dir.path(), "plan-a").unwrap();
        assert!(!path.exists());
        assert!(disk::path(dir.path(), Some("../escape")).is_err());
    }
}
