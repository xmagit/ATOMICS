//! Native preview discovery and forwarding, independent of a particular view.
pub mod client;
mod detect;
pub mod relay;

use atomics_core::preview::{MAX_PREVIEWS, Preview};
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Catalog {
    pub declared: HashMap<String, Vec<Preview>>,
    detected: HashMap<String, Vec<Preview>>,
    checked: Option<Instant>,
    reading: Option<Receiver<HashMap<String, Vec<Preview>>>>,
}

impl Catalog {
    pub fn list(&self, session: &str) -> Vec<Preview> {
        let mut ports = self.declared.get(session).cloned().unwrap_or_default();
        for preview in self.detected.get(session).into_iter().flatten() {
            if ports.len() < MAX_PREVIEWS && !ports.iter().any(|p| p.port == preview.port) {
                ports.push(preview.clone());
            }
        }
        ports.sort_by_key(|p| p.port);
        ports.truncate(MAX_PREVIEWS);
        ports
    }

    pub fn tick(&mut self, roots: Vec<(String, Option<u32>)>, wake: crate::host::Wake) -> bool {
        let live = |id: &String| roots.iter().any(|(s, _)| s == id);
        self.declared.retain(|id, _| live(id));
        self.detected.retain(|id, _| live(id));
        let mut changed = false;
        if let Some(rx) = &self.reading {
            match rx.try_recv() {
                Ok(mut data) => {
                    data.retain(|id, _| live(id));
                    changed = self.detected != data;
                    self.detected = data;
                    self.reading = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.reading = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if cfg!(target_os = "linux")
            && self.reading.is_none()
            && self.checked.is_none_or(|t| t.elapsed() >= Duration::from_secs(2))
        {
            self.checked = Some(Instant::now());
            let (tx, rx) = mpsc::sync_channel(1);
            self.reading = Some(rx);
            let _ = std::thread::Builder::new().name("preview-discovery".into()).spawn(move || {
                let data = detect::discover(&roots);
                let _ = tx.send(data);
                wake.notify();
            });
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_does_not_displace_declared_ports_at_the_limit() {
        let declared: Vec<_> = (6000..6000 + MAX_PREVIEWS as u16)
            .map(|port| Preview { port, path: "/demo".into(), label: "Web".into() })
            .collect();
        let detected: Vec<_> = (5000..5000 + MAX_PREVIEWS as u16)
            .map(|port| Preview { port, path: String::new(), label: String::new() })
            .collect();
        let catalog = Catalog {
            declared: HashMap::from([("s".into(), declared.clone())]),
            detected: HashMap::from([("s".into(), detected)]),
            ..Catalog::default()
        };
        assert_eq!(catalog.list("s"), declared);
    }
}
