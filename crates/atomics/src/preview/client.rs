//! Native local listeners. One open action accepts a session or no session for future machine views.
use std::collections::HashMap;
use std::io;
use std::net::TcpListener;

use super::relay::Relay;
use atomics_core::preview::Preview;
use atomics_core::wire::{MAX_TUNNELS, Message};
use atomics_ui::Command;
use atomics_web::remote::RemoteBackend;

struct Listening {
    socket: TcpListener,
    session: Option<String>,
    preview: Preview,
}

#[derive(Default)]
pub struct Client {
    listeners: HashMap<u16, Listening>,
    relay: Relay,
    next: u64,
}

impl Client {
    /// Return the browser URL after the loopback listener is ready.
    pub fn open(&mut self, session: Option<String>, preview: Preview) -> io::Result<String> {
        preview.validate().map_err(io::Error::other)?;
        if !self.listeners.contains_key(&preview.port) {
            if self.listeners.len() >= MAX_TUNNELS {
                return Err(io::Error::other("too many previews"));
            }
            let socket = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, preview.port))
                .or_else(|_| TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)))?;
            socket.set_nonblocking(true)?;
            self.listeners.insert(preview.port, Listening { socket, session, preview: preview.clone() });
        }
        Ok(preview.url(self.listeners[&preview.port].socket.local_addr()?.port()))
    }

    pub fn active(&self) -> bool {
        !self.listeners.is_empty()
    }

    pub fn pump(&mut self, remote: &RemoteBackend) {
        if !remote.tunnels() {
            self.listeners.clear();
            self.relay = Relay::default();
            remote.set_active_previews(Vec::new());
            return;
        }
        for action in remote.take_preview_actions() {
            match action {
                Command::OpenPreview { session, preview } => match self.open(session, preview) {
                    Ok(url) => {
                        std::thread::spawn(move || {
                            let _ = open_browser(&url);
                        });
                    }
                    Err(error) => remote.preview_error(format!("Could not open Preview: {error}")),
                },
                Command::StopPreview { port } => {
                    self.listeners.remove(&port);
                }
                _ => {}
            }
        }
        self.listeners.retain(|port, l| {
            (l.session.is_none() && remote.machine_previewable(*port))
                || remote.with_mirror(|m| {
                    m.sessions.values().any(|s| {
                        l.session.as_ref().is_none_or(|id| s.id == *id)
                            && s.status != atomics_core::session::AgentStatus::Exited
                            && s.previews.iter().any(|p| p.port == *port)
                    })
                })
        });
        for message in remote.take_tunnel_messages() {
            if let Message::TunnelRefused { reason, .. } = &message {
                remote.preview_error(format!("Preview: {reason}"));
            }
            let id = match &message {
                Message::TunnelOpened { id }
                | Message::TunnelRefused { id, .. }
                | Message::TunnelData { id, .. }
                | Message::TunnelAck { id, .. }
                | Message::TunnelClose { id, .. } => id.clone(),
                _ => continue,
            };
            if self.relay.receive(message).is_err() {
                self.relay.remove(&id);
                remote.send_tunnel(Message::TunnelClose { id, abort: true });
            }
        }
        for l in self.listeners.values() {
            while self.relay.len() < MAX_TUNNELS {
                match l.socket.accept() {
                    Ok((stream, _)) => {
                        self.next += 1;
                        let id = format!("preview-{}", self.next);
                        if self.relay.accept(id.clone(), l.preview.port, stream).is_ok() {
                            remote.send_tunnel(Message::TunnelOpen { id, port: l.preview.port });
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        for message in self.relay.poll(|port| self.listeners.contains_key(&port)) {
            remote.send_tunnel(message);
        }
        remote.set_active_previews(self.listeners.keys().copied().collect());
    }
}

pub fn open_browser(url: &str) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";
    std::process::Command::new(program).arg(url).spawn()?.wait()?;
    Ok(())
}
