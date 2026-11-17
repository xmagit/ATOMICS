//! A one-shot ClientHello over an authorized TCP tunnel. The browser performs its own TLS handshake.
use std::time::{Duration, Instant};

use atomics_core::preview;
use atomics_core::wire::Message;

pub(super) struct Probe {
    pub port: u16,
    pub opened: bool,
    started: Instant,
    header: Vec<u8>,
}

impl Probe {
    pub fn new(port: u16) -> Self {
        Self { port, opened: false, started: Instant::now(), header: Vec::new() }
    }

    /// A server that accepted TCP but silently discarded the ClientHello is usually a plain HTTP server.
    pub fn expired(&self) -> bool {
        self.started.elapsed() >= Duration::from_secs(5)
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
                if self.header.len() == 5 || !matches!(self.header.first(), Some(0x15 | 0x16)) {
                    Some(Some(super::client::tls_record(&self.header)))
                } else {
                    None
                }
            }
            Message::TunnelClose { .. } => Some(self.opened.then_some(false)),
            Message::TunnelRefused { .. } => Some(None),
            _ => None,
        }
    }
}
