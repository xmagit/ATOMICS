//! Shared preview descriptions and the bounded TCP wire format.
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::wire::{ErrorCode, TUNNEL_CHUNK, WireError};

/// The board session or Machine row selected when this forward was opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PreviewSource {
    Session(String),
    Machine(crate::machine::Listener),
}

pub const MAX_PREVIEWS: usize = 16;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preview {
    pub port: u16,
    /// Display this discovered listener under Other ports.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub secondary: bool,
    /// Live OS identity for an automatically discovered service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listener: Option<crate::machine::Listener>,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub label: String,
}

impl Preview {
    /// Restore the port and path without the live listener, query, fragment, or user-supplied label.
    pub fn for_storage(&self) -> Self {
        let path = self.path.split(['?', '#']).next().unwrap_or_default().to_owned();
        Self { port: self.port, path, ..Self::default() }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.port == 0 || self.path.len() > 1024 || self.label.len() > 128 {
            return Err("invalid preview port, path or label length".into());
        }
        if self.path.chars().any(|c| c.is_control() || c == '\\') {
            return Err("preview path contains an invalid character".into());
        }
        Ok(())
    }

    pub fn url(&self, local: u16) -> String {
        self.url_with(local, false)
    }

    /// The browser's address for the server forwarded to `local` on this machine. A server that speaks TLS
    /// gets `https://localhost`, the name a local development certificate is made for; any other gets
    /// `http://127.0.0.1`.
    pub fn url_with(&self, local: u16, tls: bool) -> String {
        let path = self.path.trim_start_matches('/');
        if tls { format!("https://localhost:{local}/{path}") } else { format!("http://127.0.0.1:{local}/{path}") }
    }

    pub fn title(&self) -> String {
        if self.label.is_empty() { format!("Port {}", self.port) } else { format!("{} · {}", self.label, self.port) }
    }
}

pub fn encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn decode(bytes: &str) -> Result<Vec<u8>, WireError> {
    if bytes.len() > TUNNEL_CHUNK.div_ceil(3) * 4 {
        return Err(WireError::new(ErrorCode::Oversize, "tunnel chunk is too large"));
    }
    let data = base64::engine::general_purpose::STANDARD
        .decode(bytes)
        .map_err(|_| WireError::new(ErrorCode::Malformed, "invalid tunnel base64"))?;
    if data.is_empty() || data.len() > TUNNEL_CHUNK {
        return Err(WireError::new(ErrorCode::Oversize, "invalid tunnel chunk size"));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_stay_on_the_loopback_origin() {
        for path in ["/demo?q=1", "//app.example/demo", "http://app.example/demo"] {
            let preview = Preview { port: 5173, path: path.into(), label: "Web".into(), ..Default::default() };
            preview.validate().unwrap();
            assert!(preview.url(5173).starts_with("http://127.0.0.1:5173/"));
            assert!(preview.url_with(5173, true).starts_with("https://localhost:5173/"));
        }
        assert!(
            Preview { port: 5173, path: "bad\npath".into(), label: String::new(), ..Default::default() }
                .validate()
                .is_err()
        );
    }
}
