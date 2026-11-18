//! The byte-moving half of the protocol server. A `Link` carries whole text frames between the
//! server and one client; everything above it (handshake, queues, replay) is transport-agnostic, so
//! the loopback WebSocket and a rendezvous channel (`rendezvous::channel`) serve the same session code.
//!
//! A link waiting for the client's next frame can be woken from another thread ([`Waker`]): the
//! client's outbox wakes it when something is queued, so a frame (an input echo above all) leaves
//! as soon as it is ready rather than at the next read timeout.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use atomics_core::wire::MAX_FRAME_BYTES;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket};

#[derive(Debug, PartialEq, Eq)]
pub enum LinkError {
    Closed,
    /// The peer sent a frame over the size limit. The link refused it before buffering it all.
    Oversize,
    /// The peer sent a binary frame; `atomics.v1` is JSON text.
    Binary,
    /// A send did not finish in time: the peer is not reading.
    Stalled,
    Io(String),
}

pub type LiveGrant = Arc<dyn Fn() -> bool + Send + Sync>;

pub trait Link: Send {
    /// The session role is fixed at hello. Paired transports can revoke that role at any time.
    fn live_grant(&self) -> LiveGrant {
        Arc::new(|| true)
    }
    /// Send one frame. May block, but only for this link's own bounded write time.
    fn send(&mut self, frame: &str) -> Result<(), LinkError>;
    /// Wait up to `wait` for one frame. `Ok(None)` means nothing arrived, or the link was woken.
    fn recv(&mut self, wait: Duration) -> Result<Option<String>, LinkError>;
    fn close(&mut self, reason: &str);
    /// Close with a code: 1000, or one of ATOMICS's in 4000-4999 (`atomics_core::tunnel::close`).
    /// A link without codes of its own closes as `close` does.
    fn close_with(&mut self, _code: u16, reason: &str) {
        self.close(reason);
    }
    /// What ends a `recv` wait early, for a link that supports it.
    fn waker(&self) -> Option<Arc<Waker>> {
        None
    }
}

/// Ends a link's `recv` wait from any thread. Never blocks; a wake while one is pending is folded
/// into it.
pub struct Waker {
    tx: UnixStream,
    pending: AtomicBool,
}

impl Waker {
    pub fn wake(&self) {
        if !self.pending.swap(true, Ordering::AcqRel) {
            // A full buffer means a wake is already waiting to be read.
            let _ = (&self.tx).write(&[1]);
        }
    }

    /// A waker and the end of its pipe that a wait polls.
    fn pair() -> io::Result<(Arc<Waker>, UnixStream)> {
        let (tx, rx) = UnixStream::pair()?;
        tx.set_nonblocking(true)?;
        rx.set_nonblocking(true)?;
        Ok((Arc::new(Waker { tx, pending: AtomicBool::new(false) }), rx))
    }

    /// Empty the pipe, then clear `pending`, in that order. While `pending` is set no waker
    /// writes, so the drain ends, and every wake is either:
    /// - after the clear: it sees `pending` unset and writes, so the next wait ends at once; or
    /// - before it: its byte is in the pipe (drained here, or left to end the next wait), or it
    ///   saw `pending` already set. Either way the clear, an acquiring swap that reads the end
    ///   of the wakers' chain of swaps, makes everything queued before that wake visible to the
    ///   read that follows this wait.
    ///
    /// Clearing first would lose a wake landing between the clear and the drain: its byte would
    /// be drained and `pending` left set with the pipe empty, so no later wake would write again.
    fn drain(&self, mut rx: &UnixStream) {
        let mut sink = [0u8; 64];
        while matches!(rx.read(&mut sink), Ok(n) if n > 0) {}
        self.pending.swap(false, Ordering::AcqRel);
    }
}

/// A wait that only a [`Waker`] (or its timeout) ends: what a link that is not a socket of its
/// own waits on, such as a rendezvous channel, whose frames another thread hands it.
pub struct Wait {
    rx: UnixStream,
    waker: Arc<Waker>,
}

impl Wait {
    pub fn new() -> io::Result<Self> {
        let (waker, rx) = Waker::pair()?;
        Ok(Self { rx, waker })
    }

    pub fn waker(&self) -> Arc<Waker> {
        self.waker.clone()
    }

    /// Wait up to `timeout` for a wake; anything queued before the wake is visible after it.
    pub fn wait(&mut self, timeout: Duration) {
        let mut fds = [libc::pollfd { fd: self.rx.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
        let timeout = timeout.as_millis().clamp(1, i32::MAX as u128) as libc::c_int;
        // SAFETY: `fds` is a valid array of one pollfd for the duration of the call.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 1, timeout) };
        if ready > 0 {
            self.waker.drain(&self.rx);
        }
    }
}

/// A loopback TCP stream whose reads, once `wait` is set, wait for data or a [`Waker`], whichever
/// comes first. Before that (the upgrade) it reads as a plain blocking socket, every read cut off
/// at the `deadline`, so a peer that trickles bytes cannot hold the upgrade open past it any more
/// than one that sends nothing.
pub struct Stream {
    tcp: TcpStream,
    rx: UnixStream,
    waker: Arc<Waker>,
    wait: Option<Duration>,
    deadline: Option<Instant>,
}

impl Stream {
    pub fn new(tcp: TcpStream) -> io::Result<Self> {
        let (waker, rx) = Waker::pair()?;
        Ok(Self { tcp, rx, waker, wait: None, deadline: None })
    }

    /// End every read of the upgrade at `deadline`: a read that would wait past it times out.
    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// How long a read waits for data or a wake; `None` makes reads plain blocking ones, cut off
    /// at the deadline if there is one. For a stream used outside `WsLink` (the rendezvous socket).
    pub fn set_wait(&mut self, wait: Option<Duration>) {
        self.wait = wait;
    }

    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.deadline = deadline;
    }

    pub fn waker(&self) -> Arc<Waker> {
        self.waker.clone()
    }

    fn drain_wakes(&mut self) {
        self.waker.drain(&self.rx);
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(wait) = self.wait else {
            if let Some(deadline) = self.deadline {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                self.tcp.set_read_timeout(Some(left))?;
            }
            return self.tcp.read(buf);
        };
        let mut fds = [
            libc::pollfd { fd: self.tcp.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: self.rx.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        let timeout = wait.as_millis().clamp(1, i32::MAX as u128) as libc::c_int;
        // SAFETY: `fds` is a valid array of two pollfd for the duration of the call.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            return Err(error);
        }
        if fds[1].revents != 0 {
            self.drain_wakes();
        }
        if fds[0].revents == 0 {
            return Err(io::ErrorKind::WouldBlock.into()); // woken, or the wait passed
        }
        // Readable, closed or failed: this read says which, without blocking.
        // SAFETY: `buf` is valid for writes of `buf.len()` bytes.
        let read = unsafe { libc::recv(self.tcp.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) };
        if read < 0 { Err(io::Error::last_os_error()) } else { Ok(read as usize) }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.tcp.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.tcp.flush()
    }
}

pub fn socket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME_BYTES))
        .max_frame_size(Some(MAX_FRAME_BYTES))
        .max_write_buffer_size(2 * MAX_FRAME_BYTES)
}

/// A WebSocket over an accepted loopback TCP stream.
pub struct WsLink {
    socket: WebSocket<Stream>,
}

fn would_block(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

fn map(error: tungstenite::Error) -> LinkError {
    use tungstenite::Error as E;
    use tungstenite::error::CapacityError;
    match error {
        E::ConnectionClosed | E::AlreadyClosed => LinkError::Closed,
        E::Capacity(CapacityError::MessageTooLong { .. }) => LinkError::Oversize,
        E::Io(e) if would_block(&e) => LinkError::Stalled,
        E::WriteBufferFull(_) => LinkError::Stalled,
        E::Io(e) => LinkError::Io(e.to_string()),
        other => LinkError::Io(other.to_string()),
    }
}

impl WsLink {
    pub fn new(mut socket: WebSocket<Stream>, write_timeout: Duration) -> io::Result<Self> {
        // Upgraded: from here reads wait as `recv` asks, and the upgrade's deadline is over.
        socket.get_mut().deadline = None;
        socket.get_ref().tcp.set_write_timeout(Some(write_timeout))?;
        socket.get_ref().tcp.set_nodelay(true)?;
        Ok(Self { socket })
    }
}

impl Link for WsLink {
    fn send(&mut self, frame: &str) -> Result<(), LinkError> {
        self.socket.send(Message::text(frame)).map_err(map)
    }

    fn recv(&mut self, wait: Duration) -> Result<Option<String>, LinkError> {
        self.socket.get_mut().wait = Some(wait);
        let read = self.socket.read();
        // Answering a ping queued a pong; push it out without waiting on a slow peer.
        let _ = self.socket.flush();
        match read {
            Ok(Message::Text(text)) => Ok(Some(text.as_str().to_owned())),
            Ok(Message::Binary(_)) => Err(LinkError::Binary),
            Ok(Message::Close(_)) => Err(LinkError::Closed),
            Ok(_) => Ok(None),
            Err(tungstenite::Error::Io(e)) if would_block(&e) => Ok(None),
            Err(e) => Err(map(e)),
        }
    }

    fn close(&mut self, reason: &str) {
        use tungstenite::protocol::CloseFrame;
        use tungstenite::protocol::frame::coding::CloseCode;
        let frame = CloseFrame { code: CloseCode::Policy, reason: reason.chars().take(100).collect::<String>().into() };
        let _ = self.socket.close(Some(frame));
        let _ = self.socket.flush();
        let _ = self.socket.get_ref().tcp.shutdown(std::net::Shutdown::Both);
    }

    fn waker(&self) -> Option<Arc<Waker>> {
        Some(self.socket.get_ref().waker.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::time::Instant;

    /// A `Stream` over a real loopback connection, and its peer (kept open, never written).
    fn stream() -> (Stream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (tcp, _) = listener.accept().unwrap();
        (Stream::new(tcp).unwrap(), peer)
    }

    /// Wait as `WsLink::recv` does. True when the wait ended before `limit`.
    fn woken(stream: &mut Stream, limit: Duration) -> bool {
        stream.wait = Some(limit);
        let started = Instant::now();
        let error = stream.read(&mut [0u8; 16]).expect_err("the peer never writes");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        started.elapsed() < limit
    }

    #[test]
    fn an_upgrade_read_ends_at_the_deadline_however_the_peer_trickles() {
        let (stream, mut peer) = stream();
        let mut stream = stream.with_deadline(Instant::now() + Duration::from_millis(300));
        let trickle = std::thread::spawn(move || {
            // Faster than any per-read timeout would ever notice.
            while peer.write_all(b"a").is_ok() {
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let started = Instant::now();
        let error = loop {
            match stream.read(&mut [0u8; 1]) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {} // a read that met the deadline
                Err(e) => break e,
            }
        };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5), "reads went on for {:?}", started.elapsed());
        drop(stream);
        trickle.join().unwrap();
    }

    #[test]
    fn a_wake_racing_the_drain_is_never_lost() {
        let (mut stream, _peer) = stream();
        let waker = stream.waker.clone();
        let stop = Arc::new(AtomicBool::new(false));
        // Wakes land at every point of the drain: between reading the pipe and clearing `pending`
        // above all, where a lost wake would leave `pending` set with the pipe empty for good.
        let hammer = std::thread::spawn({
            let (waker, stop) = (waker.clone(), stop.clone());
            move || {
                while !stop.load(Ordering::Acquire) {
                    waker.wake();
                }
            }
        });
        let until = Instant::now() + Duration::from_millis(300);
        let mut waits = 0u32;
        while Instant::now() < until {
            woken(&mut stream, Duration::from_millis(1));
            waits += 1;
        }
        stop.store(true, Ordering::Release);
        hammer.join().unwrap();
        woken(&mut stream, Duration::from_millis(1)); // whatever the last wakes left

        // One more wake must still end a wait at once.
        waker.wake();
        assert!(
            woken(&mut stream, Duration::from_secs(2)),
            "a wake was lost after {waits} waits; later wakes do nothing"
        );
        assert!(!stream.waker.pending.load(Ordering::Acquire), "a drained pipe leaves nothing pending");
        waker.wake();
        assert!(woken(&mut stream, Duration::from_secs(2)), "and the next one too");
    }
}
