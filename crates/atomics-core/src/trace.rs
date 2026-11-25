//! Chief MCP calls, reduced to cleaned display data before entering a bounded log.
//! Classification and ingestion are pure; only `disk` requires the native feature.

use std::borrow::Cow;
use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::hook::HookEvent;
use crate::tickets::{self, Cleaner, TicketCard, TicketConfig};

pub const MAX_ENTRY_BYTES: usize = 16 * 1024;
pub const MAX_LOG_BYTES: usize = 512 * 1024;
pub const MAX_ENTRIES: usize = 200;
pub const MAX_CARDS: usize = 25;
pub const MAX_DISK_BYTES: usize = 1024 * 1024;

/// The longest event text, in UTF-16 code units after trimming, as the conversation server counts.
pub const MAX_EVENT_UNITS: usize = 2000;

/// Trimmed, then cut to `MAX_EVENT_UNITS` including the omission note.
pub fn cap_block(text: &str) -> String {
    let text = text.trim();
    if tickets::utf16_len(text) <= MAX_EVENT_UNITS {
        return text.into();
    }
    let count = text.chars().count();
    let mut units = MAX_EVENT_UNITS;
    loop {
        let kept = tickets::utf16_prefix(text, units);
        let note = format!("(… {} more characters in ATOMICS)", count - kept.chars().count());
        let next = MAX_EVENT_UNITS - tickets::utf16_len(&note);
        // Fewer kept characters can only lengthen the note, so this settles.
        if next >= units {
            return format!("{kept}{note}");
        }
        units = next;
    }
}

/// Render only reduced trace fields. Errors, plans, URLs and raw responses are excluded.
/// Callers implement `tools = names` by clearing cards on their copy of each entry.
pub fn render_block(entries: &[TraceEntry]) -> String {
    let mut lines = Vec::new();
    for entry in entries.iter().take(10) {
        let elapsed = entry.elapsed_ms.map(|ms| format!(" · {ms} ms")).unwrap_or_default();
        lines.push(format!("{:?} {} · {} · {:?}{elapsed}", entry.op, entry.tool, entry.summary, entry.state));
        for card in entry.cards.iter().take(5) {
            lines.push(format!(
                "  {} · {}",
                card.link.key.as_deref().unwrap_or(&card.link.id),
                card.link.title.as_deref().unwrap_or("")
            ));
        }
    }
    cap_block(&lines.join("\n"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceOp {
    Attachment,
    Search,
    SemanticSearch,
    Read,
    Children,
    Related,
    Comment,
    LinkOrSync,
    Plan,
    Prompt,
    Reply,
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

/// Match exact tool names, never server-specific substrings or result shapes. Cursor names an MCP tool
/// `MCP:<tool>` without its server; ATOMICS's own tools are the `atomics_` ones.
pub fn classify(tool_name: &str, config: Option<&TicketConfig>) -> Option<Classification> {
    let (server, tool) = match tool_name.strip_prefix("MCP:") {
        Some(tool) => (if tool.starts_with("atomics_") { "atomics" } else { "mcp" }, tool),
        None => tool_name.strip_prefix("mcp__")?.split_once("__")?,
    };
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
            if config.operation(name).is_some_and(|operation| operation.http.is_none() && operation.tool == tool) {
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    /// Versioned, stable conversation identity. Empty or unversioned legacy identities remain in
    /// archives but cannot safely be attributed to the current chief after `/clear` or an upgrade.
    #[serde(default)]
    pub session_id: String,
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
    /// A turn of the chief's conversation instead of a call: what it was asked, or what it answered. Only clients
    /// that negotiated `turns` are sent these (`wire::TURNS`); the trace lane leaves them out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<Turn>,
}

/// The longest text a [`Turn`] keeps, in UTF-8 bytes.
pub const MAX_TURN_BYTES: usize = 8 * 1024;
pub const TRANSCRIPT_ASSISTANT_MESSAGE: &str = "TranscriptAssistantMessage";

/// Who spoke in a turn of the chief's conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnRole {
    /// A prompt the chief was given: the user's, a steering human's or ATOMICS's own.
    User,
    /// The chief's answer at the end of its turn.
    Chief,
}

/// One turn of the chief's conversation, cleaned of credentials and cut to [`MAX_TURN_BYTES`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    pub role: TurnRole,
    pub text: String,
}

/// The shareable chat ID in a Simple conversation's first line.
pub fn chat_scope(text: &str) -> Option<&str> {
    let id = text.lines().next()?.strip_prefix("[Chat ")?.strip_suffix(']')?;
    valid_chat_id(id).then_some(id)
}

/// Whether a prompt of the chief's is a note to it that nobody typed: one of ATOMICS's, or Claude Code's that a command
/// of its own ended.
pub fn is_note(prompt: &str) -> bool {
    prompt.starts_with("ATOMICS ") || prompt.trim_start().starts_with("<task-notification>")
}

pub fn valid_chat_id(id: &str) -> bool {
    id.strip_prefix("chat-").is_some_and(|token| token.len() == 32 && token.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Strip only a recognized chat routing line before cleaning or showing its user's words.
pub fn chat_body(text: &str) -> &str {
    if chat_scope(text).is_some() { text.split_once('\n').map_or("", |(_, body)| body) } else { text }
}

/// A prompt or an answer of the chief's own thread, as a trace entry with its [`Turn`].
fn turn(event: &HookEvent, cleaner: &Cleaner, at: u64) -> Option<TraceEntry> {
    if event.thread != crate::hook::HookThread::Main {
        return None;
    }
    let (role, text) = match event.name.as_str() {
        "UserPromptSubmit" => (TurnRole::User, event.prompt.as_deref()?),
        "Stop" | "AssistantProgress" | TRANSCRIPT_ASSISTANT_MESSAGE => {
            (TurnRole::Chief, event.last_assistant_message.as_deref()?)
        }
        _ => return None,
    };
    let text = cleaner.redacted(if role == TurnRole::User { chat_body(text) } else { text });
    let text: String = text.trim().chars().filter(|c| !c.is_control() || *c == '\n').collect();
    if text.is_empty() {
        return None;
    }
    let truncated = text.len() > MAX_TURN_BYTES;
    Some(TraceEntry {
        chat_id: None,
        session_id: call_id(&event.session_id),
        seq: 0,
        at,
        phase: TracePhase::Post,
        tool_use_id: None,
        server: String::new(),
        tool: String::new(),
        op: if role == TurnRole::User { TraceOp::Prompt } else { TraceOp::Reply },
        direct: false,
        plan_id: None,
        summary: cut(&text, MAX_TURN_BYTES),
        state: TraceState::Ok,
        elapsed_ms: None,
        cards: Vec::new(),
        plan: None,
        result_bytes: 0,
        error: None,
        hint: None,
        truncated,
        omitted: 0,
        turn: Some(Turn { role, text: cut(&text, MAX_TURN_BYTES) }),
    })
}

impl TraceEntry {
    pub fn same_call(&self, other: &Self) -> bool {
        self.session_id == other.session_id && self.tool_use_id.is_some() && self.tool_use_id == other.tool_use_id
    }

    fn started_at(&self) -> u64 {
        self.at.saturating_sub(self.elapsed_ms.unwrap_or_default())
    }

    fn complete_from(&mut self, start: &TraceEntry) {
        self.seq = start.seq;
        let created = self.plan.as_ref().and_then(|plan| plan.id.clone());
        self.plan_id = created.clone().or_else(|| start.plan_id.clone());
        self.chat_id.clone_from(&start.chat_id);
        self.elapsed_ms.get_or_insert_with(|| self.at.saturating_sub(start.at));
        if self.summary == "null" {
            self.summary.clone_from(&start.summary);
            self.op = start.op;
        }
        if self.plan.as_ref().is_some_and(|plan| plan.tasks.is_empty()) {
            self.plan.clone_from(&start.plan);
            if let (Some(plan), Some(id)) = (&mut self.plan, created) {
                plan.id = Some(id);
            }
            self.omitted = start.omitted;
        }
    }

    /// Join a completion with its archived start when the live display was still loading.
    pub fn with_start_from(mut self, history: &TraceLog, removed: &std::collections::HashSet<String>) -> Self {
        if self.phase == TracePhase::Post
            && self.elapsed_ms.is_none()
            && let Some(start) = history.entries.iter().find(|old| {
                old.phase == TracePhase::Pre
                    && old.same_call(&self)
                    && !old.plan_id.as_ref().is_some_and(|id| removed.contains(id))
            })
        {
            self.complete_from(start);
            return bounded(self);
        }
        self
    }

    pub fn belongs_to(&self, conversation: &str) -> bool {
        self.session_id == call_id(conversation)
    }
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
    // FNV-1a has a fixed definition, unlike DefaultHasher. These persisted identities must survive
    // toolchain upgrades. The prefix distinguishes them from the former unversioned hashes.
    let hash =
        id.bytes().fold(0xcbf29ce484222325_u64, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3));
    format!("call:fnv1a:{hash:016x}")
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
    entry.session_id = entry.session_id.chars().take(128).collect();
    if entry.op != TraceOp::Attachment {
        entry.summary = cut(&entry.summary, 1024);
    }
    entry.server = entry.server.chars().take(200).collect();
    entry.tool = entry.tool.chars().take(200).collect();
    entry.tool_use_id = entry.tool_use_id.map(|id| id.chars().take(200).collect());
    entry.error = entry.error.map(|error| error.chars().take(300).collect());
    entry.hint = entry.hint.map(|hint| hint.chars().take(300).collect());
    entry.plan_id = entry.plan_id.map(|id| id.chars().take(128).collect());
    if let Some(turn) = &mut entry.turn {
        turn.text = cut(&turn.text, MAX_TURN_BYTES);
    }
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

impl TraceEntry {
    pub fn cleaned(self, cleaner: &Cleaner) -> Option<Self> {
        let attachment = (self.op == TraceOp::Attachment).then(|| self.summary.clone());
        // The legacy reader derives this correlation key from the archive record's bytes.
        // A credential-shaped digest must still identify the same call after projection.
        let legacy_id = self
            .tool_use_id
            .as_ref()
            .filter(|id| {
                id.strip_prefix("legacy:")
                    .is_some_and(|digest| digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()))
            })
            .cloned();
        let chat_id = self.chat_id.as_ref().filter(|id| valid_chat_id(id) && cleaner.text(id) == **id).cloned();
        let session_id = self.session_id.clone();
        let plan_id = self.plan_id.as_ref().map(|id| cleaner.text(id));
        let cards = self.cards.iter().cloned().map(|card| cleaner.card(card)).collect();
        let value = clean_value(serde_json::to_value(Self { cards: Vec::new(), ..self }).ok()?, cleaner);
        let mut entry: Self = serde_json::from_value(value).ok()?;
        if legacy_id.is_some() {
            entry.tool_use_id = legacy_id;
        }
        if let Some(summary) = attachment {
            if let Ok(file) = serde_json::from_str(&summary) {
                entry.tool_use_id = Some(attachment_id(&file));
            }
            entry.summary = summary;
        }
        entry.chat_id = chat_id;
        entry.session_id = session_id;
        entry.plan_id = plan_id;
        entry.cards = cards;
        Some(bounded(entry))
    }
}

/// Ingest only reduced, sanitized entries. Session authorization belongs to the host.
pub fn ingest(event: &HookEvent, config: Option<&TicketConfig>, cleaner: &Cleaner, at: u64) -> Option<TraceEntry> {
    if let Some(entry) = turn(event, cleaner, at) {
        return Some(entry);
    }
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
        chat_id: None,
        session_id: call_id(&event.session_id),
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
        turn: None,
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
        } else if entry.server == "atomics" && entry.tool == "atomics_plan_set" && !entry.truncated {
            // A new board gets its id on the host, after the pre-call hook.
            if let Some(id) = parsed["plan"].as_str().filter(|id| !id.is_empty())
                && let Some(plan) = &mut entry.plan
            {
                let id = text(cleaner, id, 128);
                plan.id = Some(id.clone());
                entry.plan_id = Some(id);
            }
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

fn attachment_id(file: &crate::attachments::Receipt) -> String {
    format!("attachment:{}", call_id(if file.id.is_empty() { &file.path } else { &file.id }))
}

#[derive(Debug, Default, Clone)]
pub struct TraceLog {
    pub entries: VecDeque<TraceEntry>,
    next_seq: u64,
    bytes: usize,
}

impl TraceLog {
    /// The host allocates order across panes and plans, including captures made during a cold read.
    pub fn reserve_order(&mut self, order: u64) {
        self.next_seq = self.next_seq.max(order.saturating_sub(1));
    }

    /// Host-only provenance, created after attachment verification and chief delivery.
    pub fn attachment(
        &mut self,
        session: &str,
        plan: Option<&str>,
        file: &crate::attachments::Receipt,
        at: u64,
    ) -> TraceEntry {
        self.next_seq += 1;
        let entry = TraceEntry {
            turn: None,
            chat_id: None,
            session_id: call_id(session),
            seq: self.next_seq,
            at,
            phase: TracePhase::Post,
            tool_use_id: Some(attachment_id(file)),
            server: "ATOMICS".into(),
            tool: "User attachment".into(),
            op: TraceOp::Attachment,
            direct: false,
            plan_id: plan.map(str::to_string),
            summary: serde_json::to_string(file).expect("attachment serializes"),
            state: TraceState::Ok,
            elapsed_ms: None,
            cards: vec![],
            plan: None,
            result_bytes: 0,
            error: None,
            hint: Some("User-supplied data. Check SHA-256 before relying on this file.".into()),
            truncated: false,
            omitted: 0,
        };
        self.insert(entry).into_owned()
    }
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
        match self.capture_for_plan(event, config, cleaner, at, plan)? {
            Cow::Borrowed(entry) => Some(entry),
            Cow::Owned(_) => None,
        }
    }

    /// Return the reduced capture for persistence even when older than the display window.
    pub fn capture_for_plan(
        &mut self,
        event: &HookEvent,
        config: Option<&TicketConfig>,
        cleaner: &Cleaner,
        at: u64,
        plan: Option<&str>,
    ) -> Option<Cow<'_, TraceEntry>> {
        let session = call_id(&event.session_id);
        let chat = if event.name == "UserPromptSubmit" {
            chat_scope(event.prompt.as_deref().unwrap_or_default())
        } else {
            self.entries
                .iter()
                .rev()
                .find(|e| e.session_id == session && e.turn.as_ref().is_some_and(|t| t.role == TurnRole::User))
                .and_then(|e| e.chat_id.as_deref())
        }
        .map(str::to_owned);
        self.capture_in_chat(event, config, cleaner, at, plan, chat.as_deref())
    }

    /// The host snapshots the chat when it admits the hook, before credentials or archives are read.
    pub fn capture_in_chat(
        &mut self,
        event: &HookEvent,
        config: Option<&TicketConfig>,
        cleaner: &Cleaner,
        at: u64,
        plan: Option<&str>,
        chat: Option<&str>,
    ) -> Option<Cow<'_, TraceEntry>> {
        let mut entry = ingest(event, config, cleaner, at)?;
        entry.chat_id = chat.map(str::to_owned);
        entry.plan_id = entry.plan_id.or_else(|| plan.map(|id| cleaner.text(id)));
        // Rollout messages are consumed once by position. Equal text can be two messages.
        if event.name != TRANSCRIPT_ASSISTANT_MESSAGE
            && let Some(turn) = entry.turn.as_ref().filter(|turn| turn.role == TurnRole::Chief)
            && self
                .entries
                .iter()
                .rev()
                .find(|old| {
                    old.session_id == entry.session_id
                        && old.plan_id == entry.plan_id
                        && old.chat_id == entry.chat_id
                        && old.turn.is_some()
                })
                .and_then(|old| old.turn.as_ref())
                == Some(turn)
        {
            return None;
        }
        entry.seq = self.next_seq + 1;
        entry.tool_use_id.get_or_insert_with(|| format!("event:{}", entry.seq));
        Some(self.insert(entry))
    }

    pub fn merge(&mut self, other: TraceLog) {
        for entry in other.entries {
            let _ = self.insert(entry);
        }
        self.order_for_display();
    }

    // Older writers each started seq at one. Rebase only overlapping or out-of-order
    // sequences after sorting by call start, before exposing them to a client.
    fn order_for_display(&mut self) {
        let mut last = 0_u64;
        for entry in &mut self.entries {
            entry.seq = entry.seq.max(last.saturating_add(1));
            last = entry.seq;
        }
        self.next_seq = self.next_seq.max(last);
        self.recount();
    }

    fn recount(&mut self) {
        self.bytes = self.entries.iter().map(|entry| serde_json::to_vec(entry).expect("trace serializes").len()).sum();
        while self.bytes > MAX_LOG_BYTES {
            let old = self.entries.pop_front().expect("nonempty log");
            self.bytes -= serde_json::to_vec(&old).expect("trace serializes").len();
        }
    }

    pub fn remove_plan(&mut self, plan: &str) {
        self.entries.retain(|entry| entry.plan_id.as_deref() != Some(plan));
        self.recount();
    }

    fn insert(&mut self, mut entry: TraceEntry) -> Cow<'_, TraceEntry> {
        if let Some(index) = self.entries.iter().position(|old| old.same_call(&entry)) {
            let old = &self.entries[index];
            if old.phase == entry.phase && old.at > entry.at {
                return Cow::Borrowed(&self.entries[index]);
            }
            let mut old = self.entries.remove(index).expect("existing call");
            self.bytes -= serde_json::to_vec(&old).expect("trace serializes").len();
            match (old.phase, entry.phase) {
                (TracePhase::Pre, TracePhase::Post) => entry.complete_from(&old),
                (TracePhase::Post, TracePhase::Pre) => {
                    old.complete_from(&entry);
                    entry = old;
                }
                _ => entry.seq = old.seq,
            }
        }
        let entry = bounded(entry);
        self.next_seq = self.next_seq.max(entry.seq);
        let mut index = self
            .entries
            .iter()
            .position(|old| (old.started_at(), old.seq) > (entry.started_at(), entry.seq))
            .unwrap_or(self.entries.len());
        self.bytes += serde_json::to_vec(&entry).expect("trace serializes").len();
        self.entries.insert(index, entry);
        while self.entries.len() > MAX_ENTRIES || self.bytes > MAX_LOG_BYTES {
            if let Some(old) = self.entries.pop_front() {
                self.bytes -= serde_json::to_vec(&old).expect("trace serializes").len();
                if index == 0 {
                    return Cow::Owned(old);
                }
                index -= 1;
            }
        }
        Cow::Borrowed(&self.entries[index])
    }
}

#[cfg(feature = "native")]
pub mod disk {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::io::{self, Read, Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};

    fn valid_plan(name: &str) -> bool {
        !name.is_empty() && name.len() <= 128 && name.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
    }

    pub(crate) fn archive_path(home: &Path, plan: Option<&str>) -> PathBuf {
        let dir = home.join("trace");
        match plan {
            None => dir.join("special/chief.jsonl"),
            Some(name) => {
                let stem = if name.bytes().any(|c| c.is_ascii_uppercase()) {
                    format!("{name}.{}", crate::attachments::hash(name.as_bytes()))
                } else {
                    name.to_string()
                };
                dir.join("plans").join(format!("{stem}.jsonl"))
            }
        }
    }

    pub fn destination(home: &Path, plan: Option<&str>) -> io::Result<PathBuf> {
        if plan.is_some_and(|name| !valid_plan(name)) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid trace name"));
        }
        Ok(archive_path(home, plan))
    }

    fn sources<'a>(home: &Path, plan: Option<&'a str>) -> [(PathBuf, Option<&'a str>); 3] {
        let dir = home.join("trace");
        [
            (dir.join(format!("{}.jsonl", plan.unwrap_or("chief"))), plan.filter(|p| !p.eq_ignore_ascii_case("chief"))),
            (dir.join("@chief.jsonl"), None),
            (dir.join("@plan-chief.jsonl"), Some("chief")),
        ]
    }

    /// Materialize the consolidated JSONL for callers that open the file directly.
    /// Interactive history reads use load_scope and never take a writer lock.
    pub fn path(home: &Path, plan: Option<&str>) -> io::Result<PathBuf> {
        let target = destination(home, plan)?;
        for (source, fallback) in sources(home, plan) {
            if source.try_exists()? {
                // Serialize only callers migrating this legacy file.
                let _lock = lock_archive(&source)?;
                if source.try_exists()? {
                    migrate(home, &source, fallback)?;
                }
            }
        }
        Ok(target)
    }

    fn visit_legacy(
        source: &Path,
        fallback: Option<&str>,
        mut visit: impl FnMut(Option<&str>, &[u8]) -> io::Result<()>,
    ) -> io::Result<Option<std::collections::HashSet<String>>> {
        use std::collections::HashSet;
        let checkpoint = source.with_extension("imported");
        let previous: HashSet<String> = match read_only(&checkpoint) {
            Ok(file) => serde_json::from_reader(file)?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => HashSet::new(),
            Err(err) => return Err(err),
        };
        let mut imported = HashSet::new();
        for record in read_archive(source)?.split_inclusive(|b| *b == b'\n') {
            let Some(line) = record.strip_suffix(b"\n").filter(|line| !line.is_empty()) else { continue };
            let digest = crate::attachments::hash(line);
            if imported.insert(digest.clone())
                && !previous.contains(&digest)
                && let Ok(entry) = serde_json::from_slice::<TraceEntry>(line)
            {
                visit(entry.plan_id.as_deref().filter(|id| valid_plan(id)).or(fallback), line)?;
            }
        }
        Ok((previous != imported).then_some(imported))
    }

    fn migrate(home: &Path, source: &Path, fallback: Option<&str>) -> io::Result<()> {
        use std::collections::{BTreeMap, HashSet};
        let mut targets = BTreeMap::<PathBuf, Vec<Vec<u8>>>::new();
        let checkpoint = visit_legacy(source, fallback, |owner, line| {
            targets.entry(archive_path(home, owner)).or_default().push(line.into());
            Ok(())
        })?;
        for (target, lines) in targets {
            let _lock = lock_archive(&target)?;
            let existing = read_archive(&target)?;
            let mut seen = HashSet::new();
            let mut merged = Vec::new();
            for line in existing.split(|b| *b == b'\n').chain(lines.iter().map(Vec::as_slice)) {
                if !line.is_empty() && seen.insert(line) {
                    merged.push(line);
                }
            }
            // Pre and Post can share a sequence. Keep the latest update last even
            // when the legacy archive was opened again after the newer one.
            merged.sort_by_cached_key(|line| {
                serde_json::from_slice::<Value>(line)
                    .ok()
                    .and_then(|entry| entry.get("at")?.as_u64())
                    .unwrap_or_default()
            });
            let parent = target.parent().expect("archive directory");
            let mut replacement = tempfile::NamedTempFile::new_in(parent)?;
            // A legacy process may rotate and keep writing indefinitely. Keep a bounded
            // tail of complete records so that the next import can still read its target.
            let mut bytes = 0;
            let keep = merged
                .iter()
                .rev()
                .take_while(|line| {
                    bytes += line.len() + 1;
                    bytes <= MAX_DISK_BYTES * 4
                })
                .count();
            for line in &merged[merged.len() - keep..] {
                replacement.write_all(line)?;
                replacement.write_all(b"\n")?;
            }
            replacement.as_file().sync_all()?;
            replacement.persist(&target).map_err(|e| e.error)?;
            fs::File::open(parent)?.sync_all()?;
        }
        // A released writer can still hold the source inode. Leave it in place and publish the
        // checkpoint only after every destination is durable. An interrupted import can replay.
        if let Some(imported) = checkpoint {
            let parent = source.parent().expect("archive directory");
            let mut temp = tempfile::NamedTempFile::new_in(parent)?;
            serde_json::to_writer(&mut temp, &imported)?;
            temp.as_file().sync_all()?;
            temp.persist(source.with_extension("imported")).map_err(|e| e.error)?;
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    }

    fn read_archive(path: &Path) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        // A merge may combine several individually bounded legacy archives.
        let max = MAX_DISK_BYTES * 4;
        match read_only(path) {
            Ok(file) => {
                file.take(max as u64 + 1).read_to_end(&mut bytes)?;
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(bytes),
            Err(err) => return Err(err),
        }
        if bytes.len() > max {
            return Err(io::Error::other("archive exceeds the migration limit"));
        }
        Ok(bytes)
    }

    fn read_only(path: &Path) -> io::Result<fs::File> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options.open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("Expected a trace file"));
        }
        Ok(file)
    }

    fn lock_archive(path: &Path) -> io::Result<fs::File> {
        let parent = path.parent().ok_or_else(|| io::Error::other("trace directory missing"))?;
        // The lock inode survives rotation and migration of the data file.
        let name = path.file_name().ok_or_else(|| io::Error::other("trace filename missing"))?;
        let lock = open(&parent.join(name).with_extension("lock"))?;
        lock.lock()?;
        Ok(lock)
    }

    fn create_directory(path: &Path) -> io::Result<()> {
        if path.is_dir() || path.as_os_str().is_empty() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            create_directory(parent)?;
        }
        match fs::create_dir(path) {
            Ok(()) => sync_directory(path),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
            Err(err) => Err(err),
        }
    }

    fn open(path: &Path) -> io::Result<std::fs::File> {
        let parent = path.parent().ok_or_else(|| io::Error::other("trace directory missing"))?;
        create_directory(parent)?;
        let mut options = OpenOptions::new();
        options.create(true).read(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            // Restrict an existing directory and file as well as newly created ones.
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
    pub fn append(path: &Path, entry: &TraceEntry) -> io::Result<bool> {
        let mut line = serde_json::to_vec(entry)?;
        if line.len() > MAX_ENTRY_BYTES {
            return Err(io::Error::other("trace entry too large"));
        }
        line.push(b'\n');
        let _lock = lock_archive(path)?;
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
            replacement.as_file().sync_all()?;
            drop(file);
            replacement.persist(path).map_err(|error| error.error)?;
            sync_directory(path)?;
            Ok(true)
        } else {
            file.write_all(&line)?;
            file.sync_all()?;
            if len == 0 {
                sync_directory(path)?;
            }
            Ok(false)
        }
    }

    fn sync_directory(path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    }

    pub fn load(path: &Path, cleaner: &Cleaner) -> io::Result<TraceLog> {
        load_for_session(path, cleaner, None)
    }

    pub fn load_for_session(path: &Path, cleaner: &Cleaner, session: Option<&str>) -> io::Result<TraceLog> {
        let mut log = TraceLog::default();
        let session = session.map(call_id);
        visit_records(path, |line| read_entry(line, cleaner, session.as_deref(), &mut log))?;
        log.order_for_display();
        Ok(log)
    }

    pub fn load_scope(home: &Path, plan: Option<&str>, cleaner: &Cleaner, conversation: &str) -> io::Result<TraceLog> {
        load_for_session(&destination(home, plan)?, cleaner, Some(conversation))
    }

    fn scope(path: &Path) -> Option<(&Path, Option<&str>)> {
        let dir = path.parent()?;
        let home = dir.parent()?.parent()?;
        let stem = path.file_stem()?.to_str()?;
        let plan = match dir.file_name()?.to_str()? {
            "special" => None,
            "plans" => Some(stem.split('.').next()?),
            _ => return None,
        };
        (archive_path(home, plan) == path).then_some((home, plan))
    }

    /// Trace visibility and attachment retention use precisely the same complete records.
    pub(crate) fn visit_records(path: &Path, mut visit: impl FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
        if let Some((home, plan)) = scope(path) {
            for (source, fallback) in sources(home, plan) {
                if source.try_exists()? {
                    visit_legacy(&source, fallback, |owner, line| {
                        if owner == plan {
                            visit(line)?;
                        }
                        Ok(())
                    })?;
                }
            }
        }
        for record in read_archive(path)?.split_inclusive(|b| *b == b'\n') {
            if let Some(line) = record.strip_suffix(b"\n") {
                visit(line)?;
            }
        }
        Ok(())
    }

    fn read_entry(line: &[u8], cleaner: &Cleaner, session: Option<&str>, log: &mut TraceLog) -> io::Result<()> {
        if line.len() > MAX_ENTRY_BYTES {
            return Ok(());
        }
        let Ok(entry) = serde_json::from_slice::<TraceEntry>(line) else { return Ok(()) };
        if session.is_some_and(|id| entry.session_id != id) {
            return Ok(());
        }
        if let Some(mut entry) = entry.cleaned(cleaner) {
            entry.tool_use_id.get_or_insert_with(|| format!("legacy:{}", crate::attachments::hash(line)));
            let _ = log.insert(entry);
        }
        Ok(())
    }

    /// A completed thread can be opened long after the chief or its live trace has changed.
    /// Keep the reply within one trace page and restrict it to conversation entries for this exact plan.
    pub fn recent_turns(home: &Path, plan: &str, cleaner: &Cleaner) -> Vec<TraceEntry> {
        destination(home, Some(plan)).and_then(|file| last_turns(&file, plan, cleaner, None, 4)).unwrap_or_default()
    }

    /// The selected board's conversation, even after other boards have displaced its turns from the live feed.
    pub fn board_turns(home: &Path, plan: &str, cleaner: &Cleaner) -> Vec<TraceEntry> {
        destination(home, Some(plan))
            .and_then(|file| last_turns(&file, plan, cleaner, None, MAX_ENTRIES))
            .unwrap_or_default()
    }

    fn last_turns(
        file: &Path,
        plan: &str,
        cleaner: &Cleaner,
        latest: Option<&TraceEntry>,
        limit: usize,
    ) -> io::Result<Vec<TraceEntry>> {
        let mut turns = VecDeque::new();
        // Sequence numbers restart with a new chief. The private file preserves delivery order.
        let mut keep = |entry: TraceEntry| {
            if entry.turn.is_some() && entry.plan_id.as_deref() == Some(plan) {
                if turns.len() == limit {
                    turns.pop_front();
                }
                turns.push_back(entry);
            }
        };
        visit_records(file, |line| {
            if line.len() <= MAX_ENTRY_BYTES
                && let Ok(entry) = serde_json::from_slice::<TraceEntry>(line)
                && let Some(entry) = entry.cleaned(cleaner)
            {
                keep(entry);
            }
            Ok(())
        })?;
        if let Some(entry) = latest {
            keep(entry.clone());
        }
        Ok(turns.into())
    }

    /// Closing a board releases its tool trace. Only its recent cleaned conversation survives.
    pub fn save_completed_turns(
        home: &Path,
        plan: &str,
        cleaner: &Cleaner,
        latest: Option<&TraceEntry>,
    ) -> io::Result<()> {
        let file = path(home, Some(plan))?;
        let mut saved = Vec::new();
        for entry in last_turns(&file, plan, cleaner, latest, 4)? {
            serde_json::to_writer(&mut saved, &entry)?;
            saved.push(b'\n');
        }
        if saved.is_empty() {
            return remove(home, plan);
        }
        crate::store::write_private(&file, &saved)
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

    #[test]
    fn live_chief_progress_is_scoped_cleaned_and_not_repeated_at_stop() {
        let make = |name, text| {
            HookEvent::from_payload(&json!({
                "hook_event_name": name, "session_id": "main", "last_assistant_message": text
            }))
            .unwrap()
        };
        let cleaner = Cleaner::new(["private-example".to_owned()]);
        let mut log = TraceLog::default();
        let progress = make("AssistantProgress", "Checking private-example.");
        let entry = log.ingest_for_plan(&progress, None, &cleaner, 1, Some("guide")).unwrap();
        assert_eq!(entry.plan_id.as_deref(), Some("guide"));
        let text = &entry.turn.as_ref().unwrap().text;
        assert!(text.contains("Checking") && !text.contains("private-example"));
        assert!(log.ingest_for_plan(&progress, None, &cleaner, 2, Some("guide")).is_none());
        assert!(
            log.ingest_for_plan(&make("Stop", "Checking private-example."), None, &cleaner, 3, Some("guide")).is_none()
        );
        let spawned = HookEvent { thread: crate::hook::HookThread::Spawned, ..progress };
        assert!(log.ingest_for_plan(&spawned, None, &cleaner, 4, Some("guide")).is_none());
    }

    #[test]
    fn the_chiefs_prompts_and_answers_become_turns_cleaned_and_bounded() {
        let cleaner = Cleaner::default();
        let event = |value: Value| HookEvent::from_payload(&value).unwrap();
        let prompt = event(json!({"hook_event_name":"UserPromptSubmit", "session_id":"chief", "transcript_path":"/t",
            "prompt":"Plan the week. token=ghp_abcdefghijklmnopqrstuvwxyz0123456789"}));
        let entry = ingest(&prompt, None, &cleaner, 5).unwrap();
        let turn = entry.turn.as_ref().unwrap();
        assert_eq!(turn.role, TurnRole::User);
        assert!(turn.text.starts_with("Plan the week.") && !turn.text.contains("ghp_abcdef"), "{}", turn.text);
        assert_eq!((entry.at, entry.tool.as_str()), (5, ""));
        let answer = event(json!({"hook_event_name":"Stop", "session_id":"chief", "transcript_path":"/t",
            "last_assistant_message": "é".repeat(MAX_TURN_BYTES)}));
        let entry = ingest(&answer, None, &cleaner, 6).unwrap();
        let turn = entry.turn.unwrap();
        assert_eq!(turn.role, TurnRole::Chief);
        assert!(turn.text.len() <= MAX_TURN_BYTES && entry.truncated);
        // A subagent's prompt is not the chief's conversation, and an empty answer says nothing.
        let spawned = event(json!({"hook_event_name":"UserPromptSubmit", "session_id":"chief", "transcript_path":"/t",
            "agent_id":"a1", "prompt":"look around"}));
        assert!(ingest(&spawned, None, &cleaner, 7).is_none());
        let empty = event(json!({"hook_event_name":"Stop", "session_id":"chief", "last_assistant_message":"  "}));
        assert!(ingest(&empty, None, &cleaner, 8).is_none());
        // In a log, each turn is an entry of its own.
        let mut log = TraceLog::default();
        log.ingest(&prompt, None, &cleaner, 1).unwrap();
        log.ingest(&prompt, None, &cleaner, 2).unwrap();
        assert_eq!(log.entries.len(), 2);
    }
    use crate::tickets::{FieldPaths, Operation, TicketLink};

    #[test]
    #[cfg(feature = "native")]
    fn completed_history_keeps_append_order_across_chief_session_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let path = disk::path(dir.path(), Some("board")).unwrap();
        let cleaner = Cleaner::default();
        for at in 1..=6 {
            let mut log = TraceLog::default();
            let session = format!("chief-{at}");
            let event = HookEvent::from_payload(
                &json!({"hook_event_name":"Stop","session_id":session,"last_assistant_message":"A long reply. ".repeat(800)}),
            )
            .unwrap();
            let entry = log.ingest_for_plan(&event, None, &cleaner, at, Some("board")).unwrap();
            assert_eq!(entry.seq, 1);
            if at <= 5 {
                disk::append(&path, entry).unwrap();
            } else {
                assert_eq!(disk::board_turns(dir.path(), "board", &cleaner).len(), 5);
                disk::save_completed_turns(dir.path(), "board", &cleaner, Some(entry)).unwrap();
            }
        }
        let saved = disk::recent_turns(dir.path(), "board", &cleaner);
        assert_eq!(saved.iter().map(|e| e.at).collect::<Vec<_>>(), [3, 4, 5, 6]);
        let archive = std::fs::read_to_string(path).unwrap();
        assert_eq!(archive.lines().count(), 4);
        assert!(archive.len() <= 4 * (MAX_ENTRY_BYTES + 1));
        assert!(saved.iter().all(|e| e.turn.as_ref().unwrap().text.len() <= MAX_TURN_BYTES));
    }

    #[test]
    #[cfg(feature = "native")]
    fn p5_review_trace_keys_are_stable_and_legacy_entries_remain_archived() {
        assert_eq!(call_id("hello"), "call:fnv1a:a430d84680aabd0b");
        let cleaner = Cleaner::default();
        let mut log = TraceLog::default();
        let current = log
            .ingest(
                &event("PostToolUse", "call", "mcp__atomics__atomics_wait", Value::Null, Value::Null),
                None,
                &cleaner,
                1,
            )
            .unwrap()
            .clone();
        let dir = tempfile::tempdir().unwrap();
        let path = disk::path(dir.path(), Some("legacy")).unwrap();
        let mut legacy = current.clone();
        legacy.seq = 42;
        legacy.session_id.clear();
        disk::append(&path, &legacy).unwrap();
        legacy.seq = 43;
        legacy.session_id = "call:0123456789abcdef".into();
        disk::append(&path, &legacy).unwrap();
        disk::append(&path, &current).unwrap();
        let saved = std::fs::read(&path).unwrap();
        let scoped = disk::load_for_session(&path, &cleaner, Some("chief-session")).unwrap();
        assert_eq!(scoped.entries.len(), 1);
        assert_eq!(scoped.entries[0].session_id, current.session_id);
        assert_eq!(disk::load(&path, &cleaner).unwrap().entries.len(), 3);
        assert_eq!(std::fs::read(path).unwrap(), saved, "loading never discards archived calls");
    }

    #[test]
    #[cfg(feature = "native")]
    fn legacy_call_identity_survives_trace_projection() {
        let dir = tempfile::tempdir().unwrap();
        let path = disk::path(dir.path(), None).unwrap();
        let mut entry = ingest(
            &event("PostToolUse", "call", "mcp__example__lookup", Value::Null, Value::Null),
            None,
            &Cleaner::default(),
            1,
        )
        .unwrap();
        entry.tool_use_id = None;
        disk::append(&path, &entry).unwrap();
        let log = disk::load(&path, &Cleaner::default()).unwrap();
        let archived = &log.entries[0];
        assert!(archived.tool_use_id.as_deref().unwrap().starts_with("legacy:"));
        let projected = archived.clone().cleaned(&Cleaner::default()).unwrap();
        assert!(archived.same_call(&projected));
    }

    #[cfg(feature = "native")]
    fn attachment_entry(log: &mut TraceLog, plan: Option<&str>, path: &str) -> TraceEntry {
        let file = crate::attachments::Receipt {
            id: path.into(),
            at: 1,
            path: path.into(),
            file: crate::attachments::FileInfo { name: "file".into(), size: 0, sha256: crate::attachments::hash(b"") },
        };
        log.attachment("conversation", plan, &file, 1)
    }

    #[cfg(feature = "native")]
    #[test]
    fn attachment_trace_reloads_the_whole_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let archive = disk::path(dir.path(), None).unwrap();
        let entry = attachment_entry(&mut TraceLog::default(), None, &"component/".repeat(180));
        disk::append(&archive, &entry).unwrap();
        let loaded = disk::load(&archive, &Cleaner::default()).unwrap();
        assert_eq!(loaded.entries[0].summary, entry.summary);
    }

    #[cfg(feature = "native")]
    #[test]
    fn open_legacy_writer_keeps_its_last_event() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("trace/chief.jsonl");
        let mut log = TraceLog::default();
        disk::append(&legacy, &attachment_entry(&mut log, None, "first")).unwrap();
        let mut old_writer = std::fs::OpenOptions::new().append(true).open(&legacy).unwrap();
        disk::path(dir.path(), None).unwrap();
        let late = attachment_entry(&mut log, None, "late");
        writeln!(old_writer, "{}", serde_json::to_string(&late).unwrap()).unwrap();
        old_writer.sync_all().unwrap();
        let target = disk::path(dir.path(), None).unwrap();
        let log = disk::load_for_session(&target, &Cleaner::default(), Some("conversation")).unwrap();
        assert_eq!(log.entries.len(), 2);
        assert_eq!(log.entries[1], late);
    }

    #[cfg(feature = "native")]
    #[test]
    fn two_generations_keep_distinct_calls_with_the_same_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let target = disk::path(dir.path(), None).unwrap();
        let legacy = dir.path().join("trace/chief.jsonl");
        for (file, id) in [(&target, "current"), (&legacy, "legacy")] {
            let event = event("PostToolUse", id, "mcp__atomics__atomics_wait", Value::Null, Value::Null);
            let mut log = TraceLog::default();
            disk::append(file, log.ingest(&event, None, &Cleaner::default(), 1).unwrap()).unwrap();
        }
        disk::path(dir.path(), None).unwrap();
        let loaded = disk::load_for_session(&target, &Cleaner::default(), Some("chief-session")).unwrap();
        assert_eq!(loaded.entries.len(), 2);
    }

    #[cfg(feature = "native")]
    #[test]
    fn trace_scope_load_reads_a_legacy_archive_without_materializing_an_import() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("trace/chief.jsonl");
        let mut log = TraceLog::default();
        disk::append(&legacy, &attachment_entry(&mut log, None, "old")).unwrap();
        let bytes = std::fs::read(&legacy).unwrap();
        let read = disk::load_scope(dir.path(), None, &Cleaner::default(), "conversation").unwrap();
        assert_eq!(read.entries.len(), 1);
        assert_eq!(std::fs::read(&legacy).unwrap(), bytes);
        assert!(!legacy.with_extension("imported").exists());
        assert!(!dir.path().join("trace/special").exists());
    }

    #[cfg(feature = "native")]
    #[test]
    fn trace_retains_recent_calls_across_independent_writer_sequences() {
        let dir = tempfile::tempdir().unwrap();
        let target = disk::path(dir.path(), None).unwrap();
        let legacy = dir.path().join("trace/chief.jsonl");
        let mut current = TraceLog::default();
        let append = |log: &mut TraceLog, file: &std::path::Path, at: usize| {
            let event =
                event("PostToolUse", &format!("call-{at}"), "mcp__atomics__atomics_wait", Value::Null, Value::Null);
            disk::append(file, log.ingest(&event, None, &Default::default(), at as u64).unwrap()).unwrap();
        };
        for i in 0..MAX_ENTRIES {
            append(&mut current, &target, i);
        }
        append(&mut TraceLog::default(), &legacy, MAX_ENTRIES);
        disk::path(dir.path(), None).unwrap();
        let load = || disk::load(&target, &Default::default()).unwrap();
        let loaded = load();
        assert_eq!(loaded.recent(1)[0].at, MAX_ENTRIES as u64);
        assert_eq!(loaded.entries.iter().map(|e| e.seq).collect::<std::collections::HashSet<_>>().len(), MAX_ENTRIES);
        append(&mut current, &target, MAX_ENTRIES + 1);
        assert_eq!(load().recent(2)[0].at, MAX_ENTRIES as u64);
    }

    #[cfg(feature = "native")]
    #[test]
    fn trace_completion_survives_a_legacy_start_in_the_same_millisecond() {
        let dir = tempfile::tempdir().unwrap();
        let target = disk::path(dir.path(), None).unwrap();
        let legacy = dir.path().join("trace/chief.jsonl");
        for (path, name) in [(&target, "PostToolUse"), (&legacy, "PreToolUse")] {
            let event = event(name, "same", "mcp__atomics__atomics_wait", Value::Null, Value::Null);
            disk::append(path, TraceLog::default().ingest(&event, None, &Default::default(), 1).unwrap()).unwrap();
        }
        disk::path(dir.path(), None).unwrap();
        let log = disk::load(&target, &Default::default()).unwrap();
        assert_eq!(log.entries.len(), 1);
        assert_eq!(log.entries[0].state, TraceState::Ok);
    }

    #[cfg(feature = "native")]
    #[test]
    fn attachment_archive_merges_later_legacy_writes_once() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("trace/chief.jsonl");
        let mut log = TraceLog::default();
        let one = attachment_entry(&mut log, None, "first");
        let two = attachment_entry(&mut log, None, "second");
        disk::append(&legacy, &one).unwrap();
        let target = disk::path(dir.path(), None).unwrap();
        disk::append(&legacy, &one).unwrap();
        disk::append(&legacy, &two).unwrap();
        disk::path(dir.path(), None).unwrap();
        assert_eq!(disk::load(&target, &Cleaner::default()).unwrap().entries.len(), 2);
        assert_eq!(std::fs::read_to_string(target).unwrap().lines().count(), 2);
    }

    #[cfg(feature = "native")]
    #[test]
    fn removing_a_plan_trace_consumes_its_existing_legacy_records() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("trace/plan.jsonl");
        let mut log = TraceLog::default();
        disk::append(&legacy, &attachment_entry(&mut log, Some("plan"), "old")).unwrap();
        disk::remove(dir.path(), "plan").unwrap();
        assert!(!disk::path(dir.path(), Some("plan")).unwrap().exists());
        let new = attachment_entry(&mut log, Some("plan"), "new");
        disk::append(&legacy, &new).unwrap();
        let path = disk::path(dir.path(), Some("plan")).unwrap();
        assert_eq!(disk::load(&path, &Cleaner::default()).unwrap().entries, [new]);
    }

    #[cfg(feature = "native")]
    #[test]
    fn attachment_archive_keeps_plan_files_in_their_own_directory() {
        let dir = tempfile::tempdir().unwrap();
        let plan = disk::path(dir.path(), Some("chief")).unwrap();
        let chief = disk::path(dir.path(), None).unwrap();
        assert_ne!(plan.parent(), chief.parent());
        assert_eq!(plan.parent(), disk::path(dir.path(), Some("work")).unwrap().parent());
    }

    #[cfg(feature = "native")]
    #[test]
    fn attachment_archive_keeps_case_variant_plans_when_opening_chief() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("trace/chief.jsonl");
        let mut log = TraceLog::default();
        let entry = attachment_entry(&mut log, Some("Chief"), "file");
        disk::append(&legacy, &entry).unwrap();
        let unscoped = disk::path(dir.path(), None).unwrap();
        assert!(disk::load(&unscoped, &Cleaner::default()).unwrap().entries.is_empty());
        let target = disk::path(dir.path(), Some("Chief")).unwrap();
        assert_eq!(disk::load(&target, &Cleaner::default()).unwrap().entries[0], entry);
    }

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
            result: Box::new(FieldPaths { id: "id".into(), title: "subject".into(), ..Default::default() }),
            ..Default::default()
        });
        config
    }

    #[test]
    fn trace_mirror_block_limits_entries_cards_and_unicode_characters() {
        let mut entry = ingest(
            &event("PostToolUse", "a", "mcp__atomics__atomics_ticket_read", json!({"query":"safe"}), json!({})),
            None,
            &Cleaner::default(),
            10,
        )
        .unwrap();
        entry.cards = (0..9).map(card).collect();
        entry.elapsed_ms = Some(12);
        let block = render_block(&[entry.clone()]);
        assert!(block.contains("Read") && block.contains("Ok") && block.contains("12 ms"));
        assert!(block.contains("DEMO-4") && !block.contains("DEMO-5"));
        entry.cards.clear();
        entry.summary.clear();
        assert_eq!(render_block(&vec![entry; 15]).lines().count(), 10);
        for length in [2000, 2001, 2964, 3000, 10000] {
            let text = "é".repeat(length);
            let capped = cap_block(&text);
            assert_eq!(capped.chars().count(), 2000);
            if length > 2000 {
                let kept = capped.chars().take_while(|c| *c == 'é').count();
                assert!(capped.ends_with(&format!("(… {} more characters in ATOMICS)", length - kept)));
            }
        }
        // Emoji are two UTF-16 units each; the limit counts units after trimming.
        let emoji = "\u{1f600}";
        let exact = format!("  {}\n", emoji.repeat(1000));
        assert_eq!(cap_block(&exact), emoji.repeat(1000));
        for count in [1001, 1500, 4000] {
            let capped = cap_block(&emoji.repeat(count));
            let units = capped.encode_utf16().count();
            assert!((1999..=2000).contains(&units), "{count}: {units}");
            let kept = capped.chars().take_while(|c| c.to_string() == emoji).count();
            assert!(capped.ends_with(&format!("(… {} more characters in ATOMICS)", count - kept)), "{capped}");
        }
        assert_eq!(cap_block(&format!("{}\u{1f600}", "a".repeat(1999))).encode_utf16().count(), 2000);
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
        // Cursor names an MCP tool without its server.
        let cursor = classify("MCP:atomics_plan_set", None).unwrap();
        assert_eq!(
            (cursor.server.as_str(), cursor.tool.as_str(), cursor.op),
            ("atomics", "atomics_plan_set", TraceOp::Plan)
        );
        assert_eq!(classify("MCP:atomics_ticket_read", None).unwrap().op, TraceOp::Read);
        let direct = classify("MCP:find", Some(&config)).unwrap();
        assert_eq!((direct.op, direct.direct), (TraceOp::Search, true));
        assert!(classify("MCP:", None).is_none());
    }

    #[test]
    fn a_new_plan_card_uses_the_board_id_returned_by_the_host() {
        let mut log = TraceLog::default();
        let pre = event(
            "PreToolUse",
            "create",
            "mcp__atomics__atomics_plan_set",
            json!({"goal":"Field guide", "tasks":[{"id":"coast", "title":"Coast page"}]}),
            Value::Null,
        );
        let cleaner = Cleaner::default();
        log.ingest_for_plan(&pre, None, &cleaner, 1, Some("previous")).unwrap();
        let post = HookEvent {
            name: "PostToolUse".into(),
            tool_input: Value::Null,
            tool_response: json!([{"type":"text","text":"{\"plan\":\"field-guide-2\"}"}]),
            ..pre
        };
        let entry = log.ingest_for_plan(&post, None, &cleaner, 2, None).unwrap();
        assert_eq!(entry.plan.as_ref().and_then(|p| p.id.as_deref()), Some("field-guide-2"));
        assert_eq!(entry.plan_id.as_deref(), Some("field-guide-2"));
        assert_eq!(entry.plan.as_ref().unwrap().tasks.len(), 1);
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
            post.tool_use_id = Some(format!("small-{i}"));
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
            post.tool_use_id = Some(format!("medium-{i}"));
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
            post.tool_use_id = Some(format!("archive-{i}"));
            disk::append(&path, log.ingest(&post, None, &Cleaner::default(), i).unwrap()).unwrap();
        }
        assert!(std::fs::metadata(&path).unwrap().len() < MAX_DISK_BYTES as u64);
        assert!(std::fs::read_to_string(&path).unwrap().lines().count() < 200);
        post.name = "PreToolUse".into();
        post.tool_use_id = Some("final".into());
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
