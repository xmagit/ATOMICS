use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use atomics_core::config::ChiefAgent;
use serde_json::Value;

const MAX_RECORD_BYTES: usize = 1024 * 1024;
/// How far back a resumed transcript is read for the size of its conversation.
const TAIL_BYTES: u64 = 512 * 1024;

/// Follow the chief's append-only transcript on the host where it runs.
pub(in crate::app) struct ChiefTranscript {
    pub(super) conversation: String,
    provider: ChiefAgent,
    path: PathBuf,
    resume: bool,
    reader: Option<BufReader<File>>,
    offset: u64,
    line: Vec<u8>,
    oversized: bool,
    last_message: Option<String>,
    stop_reply: Option<String>,
    /// How many tokens the conversation holds, as of its last reply.
    pub(super) context: Option<u64>,
}

impl ChiefTranscript {
    pub(super) fn new(conversation: &str, provider: ChiefAgent, path: &Path, resume: bool) -> Option<Self> {
        let name = path.file_name()?.to_str()?;
        if !crate::ipc::safe_chief_root_id(conversation) {
            return None;
        }
        match provider {
            ChiefAgent::Codex if name.starts_with("rollout-") && name.ends_with(&format!("-{conversation}.jsonl")) => {}
            ChiefAgent::Claude if name == format!("{conversation}.jsonl") => {}
            _ => return None,
        }
        Some(Self {
            conversation: conversation.into(),
            provider,
            path: path.into(),
            resume,
            reader: None,
            offset: 0,
            line: Vec::new(),
            oversized: false,
            last_message: None,
            stop_reply: None,
            context: None,
        })
    }

    pub(super) fn follows_claude(&self) -> bool {
        self.provider == ChiefAgent::Claude && self.reader.is_some()
    }

    pub(super) fn prompt(&mut self) {
        self.last_message = None;
        self.stop_reply = None;
    }

    pub(super) fn stop(&mut self, reply: Option<&str>) -> bool {
        let Some(reply) = reply else { return false };
        if self.last_message.as_deref().is_some_and(|text| text.trim() == reply.trim()) {
            return true;
        }
        self.stop_reply = Some(reply.to_owned());
        false
    }

    pub(super) fn read(&mut self) -> Vec<String> {
        if self.reader.is_none() {
            let Some(file) = self.open() else { return Vec::new() };
            self.reader = Some(BufReader::new(file));
        }
        let reader = self.reader.as_mut().unwrap();
        let Ok(metadata) = reader.get_ref().metadata() else { return Vec::new() };
        // Stop at this snapshot of the file. Codex can keep writing while the host reads.
        let end = metadata.len();
        let mut messages = Vec::new();
        while self.offset < end {
            let Ok(buffer) = reader.fill_buf() else { break };
            let bytes = &buffer[..buffer.len().min((end - self.offset) as usize)];
            if bytes.is_empty() {
                break;
            }
            let newline = bytes.iter().position(|&byte| byte == b'\n');
            let count = newline.map_or(bytes.len(), |i| i + 1);
            self.oversized |= self.line.len() + count > MAX_RECORD_BYTES;
            if !self.oversized {
                self.line.extend_from_slice(&bytes[..count]);
            }
            reader.consume(count);
            self.offset += count as u64;
            if newline.is_some() {
                let record = if self.oversized { None } else { serde_json::from_slice::<Value>(&self.line).ok() };
                let size = record.as_ref().and_then(|r| context_tokens(r, self.provider, &self.conversation));
                self.context = size.or(self.context);
                if let Some(text) =
                    record.as_ref().and_then(|record| assistant_message(record, self.provider, &self.conversation))
                {
                    self.last_message = Some(text.clone());
                    if self.stop_reply.as_deref().is_some_and(|reply| reply.trim() == text.trim()) {
                        // Stop can provide a fallback while the writer finishes flushing the line.
                        self.stop_reply = None;
                    } else {
                        messages.push(text);
                    }
                }
                self.line.clear();
                self.oversized = false;
            }
        }
        messages
    }

    fn last_context(&self, file: &mut File, end: u64) -> Option<u64> {
        use std::io::Read;
        file.seek(SeekFrom::Start(end.saturating_sub(TAIL_BYTES))).ok()?;
        let mut tail = Vec::new();
        file.take(TAIL_BYTES).read_to_end(&mut tail).ok()?;
        let mut records = tail.split(|&byte| byte == b'\n').rev();
        records.find_map(|line| context_tokens(&serde_json::from_slice(line).ok()?, self.provider, &self.conversation))
    }

    fn open(&mut self) -> Option<File> {
        let path = self.path.canonicalize().ok()?;
        let root = match self.provider {
            ChiefAgent::Codex => crate::setup::chief_codex_home().join("sessions"),
            ChiefAgent::Claude => crate::setup::agent_claude_dir().join("projects"),
            _ => return None,
        }
        .canonicalize()
        .ok()?;
        if self.provider == ChiefAgent::Claude && path.parent()?.parent()? != root {
            return None;
        }
        if !path.starts_with(root) || path.file_name() != self.path.file_name() {
            return None;
        }
        let mut file =
            OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path).ok()?;
        if !file.metadata().ok()?.is_file() {
            return None;
        }
        if self.resume {
            // The trace already has its own history. Only follow new records on resume.
            let end = file.seek(SeekFrom::End(0)).ok()?;
            self.context = self.last_context(&mut file, end);
            self.offset = file.seek(SeekFrom::Start(end)).ok()?;
        }
        Some(file)
    }
}

/// What the provider says it read and wrote for its last reply, which is what the conversation holds now.
fn context_tokens(record: &Value, provider: ChiefAgent, conversation: &str) -> Option<u64> {
    if provider == ChiefAgent::Codex {
        let payload = &record["payload"];
        return (payload["type"] == "token_count")
            .then(|| payload["info"]["last_token_usage"]["total_tokens"].as_u64())?;
    }
    if record["type"] != "assistant" || record["sessionId"] != conversation || record["isSidechain"] == true {
        return None;
    }
    let usage = &record["message"]["usage"];
    let parts = ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens", "output_tokens"];
    let total: u64 = parts.iter().filter_map(|part| usage[*part].as_u64()).sum();
    (total > 0).then_some(total)
}

fn assistant_message(record: &Value, provider: ChiefAgent, conversation: &str) -> Option<String> {
    if provider == ChiefAgent::Claude {
        return crate::hook_client::claude_assistant_text(record, conversation);
    }
    if record["type"] != "event_msg" {
        return None;
    }
    let payload = &record["payload"];
    let text = match payload["type"].as_str()? {
        "item_completed" if payload["item"]["type"] == "AgentMessage" => payload["item"]["content"]
            .as_array()?
            .iter()
            .filter(|part| part["type"] == "Text")
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        "agent_message" => payload["message"].as_str()?.to_owned(),
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(text)
}
