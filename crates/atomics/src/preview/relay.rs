//! Nonblocking TCP relay. Socket writes return credit to the peer.
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::mpsc::{self, Receiver};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use atomics_core::preview;
use atomics_core::wire::{MAX_TUNNELS, Message, TUNNEL_CHUNK, TUNNEL_WINDOW};

pub fn connect_loopback(address: SocketAddr) -> io::Result<TcpStream> {
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "preview target must be loopback"));
    }
    TcpStream::connect_timeout(&address, Duration::from_millis(500))
}

struct Connection {
    stream: Option<TcpStream>,
    connecting: Option<Receiver<io::Result<TcpStream>>>,
    opened: bool,
    port: u16,
    credit: u32,
    received: u32,
    pending: VecDeque<Vec<u8>>,
    offset: usize,
    eof: bool,
    peer_eof: bool,
    shut_write: bool,
}

impl Connection {
    fn new(port: u16, stream: Option<TcpStream>, opened: bool) -> io::Result<Self> {
        if let Some(stream) = &stream {
            stream.set_nonblocking(true)?;
            stream.set_nodelay(true)?;
        }
        Ok(Self {
            stream,
            connecting: None,
            opened,
            port,
            credit: TUNNEL_WINDOW,
            received: 0,
            pending: VecDeque::new(),
            offset: 0,
            eof: false,
            peer_eof: false,
            shut_write: false,
        })
    }

    fn poll(&mut self, id: &str, out: &mut Vec<Message>) -> io::Result<bool> {
        if let Some(rx) = &self.connecting {
            match rx.try_recv() {
                Ok(result) => {
                    let stream = result?;
                    stream.set_nonblocking(true)?;
                    stream.set_nodelay(true)?;
                    self.stream = Some(stream);
                    self.connecting = None;
                    self.opened = true;
                    out.push(Message::TunnelOpened { id: id.into() });
                }
                Err(mpsc::TryRecvError::Empty) => return Ok(false),
                Err(_) => return Err(io::Error::other("preview connection ended")),
            }
        }
        if !self.opened {
            return Ok(false);
        }
        let Some(stream) = &mut self.stream else { return Ok(false) };
        let mut written = 0;
        while let Some(front) = self.pending.front() {
            match stream.write(&front[self.offset..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.offset += n;
                    written += n as u32;
                    if self.offset == front.len() {
                        self.pending.pop_front();
                        self.offset = 0;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        if written > 0 {
            self.received -= written;
            out.push(Message::TunnelAck { id: id.into(), bytes: written });
        }
        if self.peer_eof && self.pending.is_empty() && !self.shut_write {
            stream.shutdown(Shutdown::Write)?;
            self.shut_write = true;
        }
        let mut buf = [0; TUNNEL_CHUNK];
        let mut budget = TUNNEL_WINDOW;
        while !self.eof && self.credit > 0 && budget > 0 {
            let size = buf.len().min(self.credit as usize).min(budget as usize);
            match stream.read(&mut buf[..size]) {
                Ok(0) => {
                    self.eof = true;
                    out.push(Message::TunnelClose { id: id.into(), abort: false });
                }
                Ok(n) => {
                    self.credit -= n as u32;
                    budget -= n as u32;
                    out.push(Message::TunnelData { id: id.into(), bytes: preview::encode(&buf[..n]) });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(self.eof && self.shut_write && self.credit == TUNNEL_WINDOW)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(stream) = &self.stream {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

/// How long a relay counts as busy after it last moved data: a peer polling it waits briefly meanwhile.
const BUSY_FOR: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct Relay {
    connections: HashMap<String, Connection>,
    connecting: Arc<AtomicUsize>,
    /// When data last went either way, or a connection opened.
    moved: Option<Instant>,
}

impl Relay {
    /// Whether the relay moved data lately or a connect is under way, so a loop that cannot wait on its
    /// sockets should poll it again soon rather than at its idle pace.
    pub fn busy(&self) -> bool {
        !self.connections.is_empty()
            && (self.connections.values().any(|c| c.connecting.is_some())
                || self.moved.is_some_and(|at| at.elapsed() < BUSY_FOR))
    }

    /// The sockets worth waiting on, and for what (`POLLIN`, `POLLOUT`): an open connection that may read
    /// (it has credit and its socket has not ended) or has bytes waiting to be written. A socket that may
    /// do neither is left out, so a wait on these never wakes for something the next poll cannot use.
    pub fn interest(&self) -> Vec<(RawFd, i16)> {
        self.connections
            .values()
            .filter_map(|c| {
                let stream = c.stream.as_ref().filter(|_| c.opened)?;
                let mut events = 0;
                if !c.eof && c.credit > 0 {
                    events |= libc::POLLIN;
                }
                if !c.pending.is_empty() {
                    events |= libc::POLLOUT;
                }
                (events != 0).then(|| (stream.as_raw_fd(), events))
            })
            .collect()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.connections.contains_key(id)
    }
    pub fn len(&self) -> usize {
        self.connections.len()
    }

    pub fn open(&mut self, id: String, port: u16) -> Result<(), String> {
        if self.connections.len() >= MAX_TUNNELS || self.contains(&id) {
            return Err("too many tunnels or duplicate tunnel id".into());
        }
        if self.connecting.fetch_add(1, Ordering::AcqRel) >= MAX_TUNNELS {
            self.connecting.fetch_sub(1, Ordering::AcqRel);
            return Err("too many preview connects under way".into());
        }
        let connecting = self.connecting.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("preview-connect".into())
            .spawn(move || {
                let result = connect_loopback(SocketAddr::from(([127, 0, 0, 1], port)))
                    .or_else(|_| connect_loopback(SocketAddr::new(std::net::Ipv6Addr::LOCALHOST.into(), port)));
                let _ = tx.send(result);
                connecting.fetch_sub(1, Ordering::AcqRel);
            })
            .map_err(|e| {
                self.connecting.fetch_sub(1, Ordering::AcqRel);
                e.to_string()
            })?;
        let mut connection = Connection::new(port, None, false).map_err(|e| e.to_string())?;
        connection.connecting = Some(rx);
        self.connections.insert(id, connection);
        Ok(())
    }

    pub fn accept(&mut self, id: String, port: u16, stream: TcpStream) -> io::Result<()> {
        if self.len() >= MAX_TUNNELS {
            return Err(io::Error::other("too many preview connections"));
        }
        self.connections.insert(id, Connection::new(port, Some(stream), false)?);
        Ok(())
    }

    pub fn remove(&mut self, id: &str) {
        self.connections.remove(id);
    }

    pub fn receive(&mut self, message: Message) -> Result<(), String> {
        let id = match &message {
            Message::TunnelOpened { id }
            | Message::TunnelRefused { id, .. }
            | Message::TunnelData { id, .. }
            | Message::TunnelAck { id, .. }
            | Message::TunnelClose { id, .. } => id.clone(),
            _ => return Err("invalid tunnel message".into()),
        };
        if matches!(message, Message::TunnelData { .. } | Message::TunnelAck { .. } | Message::TunnelOpened { .. }) {
            self.moved = Some(Instant::now());
        }
        let Some(c) = self.connections.get_mut(&id) else {
            return if matches!(message, Message::TunnelClose { .. } | Message::TunnelAck { .. }) {
                Ok(())
            } else {
                Err("unknown tunnel".into())
            };
        };
        match message {
            Message::TunnelRefused { .. } => {
                self.connections.remove(&id);
            }
            Message::TunnelOpened { .. } if !c.opened => c.opened = true,
            Message::TunnelData { bytes, .. } if c.opened && !c.peer_eof => {
                let data = preview::decode(&bytes).map_err(|e| e.to_string())?;
                if data.len() as u32 > TUNNEL_WINDOW - c.received {
                    return Err("tunnel exceeded receive credit".into());
                }
                c.received += data.len() as u32;
                c.pending.push_back(data);
            }
            Message::TunnelAck { bytes, .. } if bytes > 0 && bytes <= TUNNEL_WINDOW - c.credit => c.credit += bytes,
            Message::TunnelClose { abort: true, .. } => {
                self.connections.remove(&id);
            }
            Message::TunnelClose { .. } => c.peer_eof = true,
            _ => return Err("invalid tunnel state or credit".into()),
        }
        Ok(())
    }

    pub fn poll(&mut self, mut allowed: impl FnMut(u16) -> bool) -> Vec<Message> {
        let mut out = Vec::new();
        self.connections.retain(|id, c| {
            if !allowed(c.port) {
                out.push(Message::TunnelClose { id: id.clone(), abort: true });
                return false;
            }
            match c.poll(id, &mut out) {
                Ok(done) => !done,
                Err(_) => {
                    out.push(if c.connecting.is_some() {
                        Message::TunnelRefused { id: id.clone(), reason: "preview connection failed".into() }
                    } else {
                        Message::TunnelClose { id: id.clone(), abort: true }
                    });
                    false
                }
            }
        });
        if !out.is_empty() {
            self.moved = Some(Instant::now());
        }
        out
    }
}
