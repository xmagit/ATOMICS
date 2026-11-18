//! A one-shot ClientHello over an authorized TCP tunnel. The browser performs its own TLS handshake.
use std::time::{Duration, Instant};

use atomics_core::preview;
use atomics_core::wire::Message;

pub(super) struct Probe {
    pub port: u16,
    opened: bool,
    timeout: Duration,
    started: Instant,
    header: Vec<u8>,
}

impl Probe {
    pub fn new(port: u16, waiting: u64) -> Self {
        Self {
            port,
            timeout: Duration::from_secs(5 + waiting.min(55)),
            opened: false,
            started: Instant::now(),
            header: Vec::new(),
        }
    }

    /// A silent server leaves the scheme undetermined.
    pub fn expired(&self) -> bool {
        self.started.elapsed() >= self.timeout
    }

    /// None keeps reading. A completed probe either recognized a scheme or couldn't reach the service.
    pub fn receive(&mut self, frame: &Message) -> Option<Option<bool>> {
        match frame {
            Message::TunnelOpened { .. } => {
                self.opened = true;
                self.started = Instant::now();
                None
            }
            Message::TunnelData { bytes, .. } if self.opened => {
                let Ok(bytes) = preview::decode(bytes) else { return Some(None) };
                self.header.extend(bytes.into_iter().take(5 - self.header.len()));
                (self.header.len() == 5).then(|| super::client::scheme(&self.header))
            }
            Message::TunnelClose { .. } => Some(None),
            Message::TunnelRefused { .. } => Some(None),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eof_and_unrecognized_replies_leave_the_scheme_pending() {
        for reply in [&b""[..], b"hello"] {
            let mut probe = Probe::new(5173, 0);
            let id = "probe".to_string();
            probe.receive(&Message::TunnelOpened { id: id.clone() });
            let result = if reply.is_empty() {
                None
            } else {
                probe.receive(&Message::TunnelData { id: id.clone(), bytes: preview::encode(reply) })
            };
            assert_eq!(result.or_else(|| probe.receive(&Message::TunnelClose { id, abort: false })), Some(None));
        }
    }
}
