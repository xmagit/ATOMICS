//! Native preview discovery and forwarding, independent of a particular view.
pub mod client;
pub(crate) mod detect;
pub(crate) mod direct;
mod probe;
pub mod relay;
pub(crate) mod saved;

use atomics_core::preview::{MAX_PREVIEWS, Preview};
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Catalog {
    pub direct: std::sync::Arc<direct::Previews>,
    pub declared: HashMap<String, Vec<Preview>>,
    detected: HashMap<String, Vec<Preview>>,
    checked: Option<Instant>,
    reading: Option<Receiver<HashMap<String, Vec<Preview>>>>,
}

impl Catalog {
    pub fn list(&self, session: &str) -> Vec<Preview> {
        let mut ports = self.declared.get(session).cloned().unwrap_or_default();
        for preview in self.detected.get(session).into_iter().flatten() {
            if let Some(declared) = ports.iter_mut().find(|p| p.port == preview.port) {
                declared.listener = preview.listener.clone();
            } else if ports.len() < MAX_PREVIEWS {
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

/// Reserve a single browser port in every address family its URL can resolve to.
fn bind_loopbacks(preferred: u16, tls: bool) -> std::io::Result<(u16, Vec<std::net::TcpListener>)> {
    use std::{
        io,
        net::{Ipv4Addr, Ipv6Addr, TcpListener},
    };
    let bind = |requested| -> io::Result<_> {
        let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, requested))?;
        let local = v4.local_addr()?.port();
        let mut sockets = vec![v4];
        if tls {
            sockets.push(TcpListener::bind((Ipv6Addr::LOCALHOST, local))?);
        }
        for socket in &sockets {
            socket.set_nonblocking(true)?;
        }
        Ok((local, sockets))
    };
    // localhost must reach this relay in either family. An ephemeral IPv4
    // port can also be occupied on IPv6, so retry the pair on collision.
    let mut result = bind(preferred).or_else(|_| bind(0));
    for _ in 0..16 {
        if !matches!(&result, Err(e) if e.kind() == io::ErrorKind::AddrInUse) {
            break;
        }
        result = bind(0);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_does_not_displace_declared_ports_at_the_limit() {
        let declared: Vec<_> = (6000..6000 + MAX_PREVIEWS as u16)
            .map(|port| Preview { port, path: "/demo".into(), label: "Web".into(), ..Default::default() })
            .collect();
        let detected: Vec<_> = (5000..5000 + MAX_PREVIEWS as u16)
            .map(|port| Preview { port, path: String::new(), label: String::new(), ..Default::default() })
            .collect();
        let catalog = Catalog {
            declared: HashMap::from([("s".into(), declared.clone())]),
            detected: HashMap::from([("s".into(), detected)]),
            ..Catalog::default()
        };
        assert_eq!(catalog.list("s"), declared);
    }
}

/// One port on both loopbacks, for tests: others bind ports of either family at the same time.
#[cfg(test)]
pub(crate) mod test_ports {
    use std::net::{Ipv4Addr, Ipv6Addr, TcpListener};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    const TRIES: usize = 200;

    /// A listener on each loopback, on the same port.
    pub(crate) fn pair() -> (TcpListener, TcpListener) {
        (0..TRIES)
            .find_map(|_| {
                let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
                let port = v4.local_addr().unwrap().port();
                TcpListener::bind((Ipv6Addr::LOCALHOST, port)).ok().map(|v6| (v4, v6))
            })
            .expect("a port that is free on both loopbacks")
    }

    /// A listener on the IPv6 loopback alone. The socket beside it has the port on IPv4 and refuses every connection:
    /// nothing else can listen there while it lives.
    pub(crate) fn ipv6_only() -> (TcpListener, OwnedFd) {
        (0..TRIES)
            .find_map(|_| {
                let v6 = TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).unwrap();
                reserve(v6.local_addr().unwrap().port()).map(|reserved| (v6, reserved))
            })
            .expect("a port that is free on both loopbacks")
    }

    fn reserve(port: u16) -> Option<OwnedFd> {
        // SAFETY: the descriptor is checked before it is owned, and the address outlives the call.
        unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
            assert!(fd >= 0, "{}", std::io::Error::last_os_error());
            let socket = OwnedFd::from_raw_fd(fd);
            let mut address: libc::sockaddr_in = std::mem::zeroed();
            address.sin_family = libc::AF_INET as libc::sa_family_t;
            address.sin_port = port.to_be();
            address.sin_addr.s_addr = u32::from(Ipv4Addr::LOCALHOST).to_be();
            let size = std::mem::size_of_val(&address) as libc::socklen_t;
            (libc::bind(socket.as_raw_fd(), (&raw const address).cast(), size) == 0).then_some(socket)
        }
    }
}
