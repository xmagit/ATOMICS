//! Shared preview descriptions and the bounded TCP wire format.
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::wire::{ErrorCode, TUNNEL_CHUNK, WireError};

pub const MAX_PREVIEWS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preview {
    pub port: u16,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub label: String,
}

impl Preview {
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
        format!("http://127.0.0.1:{local}/{}", self.path.trim_start_matches('/'))
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
            let preview = Preview { port: 5173, path: path.into(), label: "Web".into() };
            preview.validate().unwrap();
            assert!(preview.url(5173).starts_with("http://127.0.0.1:5173/"));
        }
        assert!(Preview { port: 5173, path: "bad\npath".into(), label: String::new() }.validate().is_err());
    }
}
