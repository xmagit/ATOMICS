use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde_json::Value;

const MAX_RECORD_BYTES: usize = 1024 * 1024;

/// Follow the main Codex chief's append-only rollout on the host where it runs.
pub(in crate::app) struct CodexRollout {
    pub(super) pane: String,
    pub(super) conversation: String,
    path: PathBuf,
    resume: bool,
    reader: Option<BufReader<File>>,
    offset: u64,
    line: Vec<u8>,
    oversized: bool,
    last_message: Option<String>,
    stop_reply: Option<String>,
}

impl CodexRollout {
    pub(super) fn new(pane: &str, conversation: &str, path: &Path, resume: bool) -> Option<Self> {
        let name = path.file_name()?.to_str()?;
        if !crate::ipc::safe_chief_root_id(conversation)
            || !name.starts_with("rollout-")
            || !name.ends_with(&format!("-{conversation}.jsonl"))
        {
            return None;
        }
        Some(Self {
            pane: pane.into(),
            conversation: conversation.into(),
            path: path.into(),
            resume,
            reader: None,
            offset: 0,
            line: Vec::new(),
            oversized: false,
            last_message: None,
            stop_reply: None,
        })
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
                if !self.oversized
                    && let Some(text) = assistant_message(&self.line)
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

    fn open(&mut self) -> Option<File> {
        let path = self.path.canonicalize().ok()?;
        let root = crate::setup::chief_codex_home().join("sessions").canonicalize().ok()?;
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
            self.offset = file.seek(SeekFrom::End(0)).ok()?;
        }
        Some(file)
    }
}

fn assistant_message(line: &[u8]) -> Option<String> {
    let record = serde_json::from_slice::<Value>(line).ok()?;
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
