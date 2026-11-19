//! A desktop browser can choose either family for localhost. Pin both to the selected TLS service.
use super::relay::connect_loopback;
use async_io::Async;
use atomics_core::preview::{MAX_PREVIEWS, Preview, PreviewSource};
use atomics_core::wire::MAX_TUNNELS;
use futures_util::{AsyncReadExt, TryStreamExt, future, stream};
use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::thread::JoinHandle;

#[derive(Default)]
pub struct Previews {
    forwards: Mutex<HashMap<SocketAddr, Forward>>,
}

impl Previews {
    pub fn url(&self, source: &PreviewSource, preview: &Preview) -> io::Result<String> {
        let waiting = || io::Error::other("Waiting for the preview to identify HTTP or HTTPS. Open it again to retry.");
        let listener = match source {
            PreviewSource::Machine(row) => Some(row),
            PreviewSource::Session(_) => preview.listener.as_ref(),
        };
        let stream = if let Some(row) = listener {
            let ip = row.loopback_ip().ok_or_else(|| io::Error::other("Preview needs a loopback listener"))?;
            connect_loopback(SocketAddr::new(ip, preview.port)).map_err(|_| waiting())?
        } else {
            // An accepting socket counts even when it cannot identify its protocol.
            let v4 = connect_loopback((Ipv4Addr::LOCALHOST, preview.port).into());
            let v6 = connect_loopback((Ipv6Addr::LOCALHOST, preview.port).into());
            match (v4, v6) {
                (Ok(_), Ok(_)) => {
                    return Err(io::Error::other(
                        "Both IPv4 and IPv6 accept connections on this port. Open Machine and choose a listener.",
                    ));
                }
                (Ok(stream), Err(error)) | (Err(error), Ok(stream))
                    if matches!(error.kind(), io::ErrorKind::ConnectionRefused | io::ErrorKind::AddrNotAvailable)
                        || error.raw_os_error() == Some(libc::EAFNOSUPPORT) =>
                {
                    stream
                }
                _ => return Err(waiting()),
            }
        };
        let address = stream.peer_addr()?;
        if !super::client::speaks_tls(stream).ok_or_else(waiting)? {
            return Ok(format!("http://{address}/{}", preview.path.trim_start_matches('/')));
        }
        let mut forwards = self.forwards.lock().unwrap();
        forwards.retain(|_, forward| !forward.thread.as_ref().unwrap().is_finished());
        if let Some(forward) = forwards.get_mut(&address) {
            forward.source = source.clone();
            return Ok(preview.url_with(forward.port, true));
        }
        if forwards.len() >= MAX_PREVIEWS {
            return Err(io::Error::other("Stop an open HTTPS preview before opening another"));
        }
        let forward = Forward::start(address, source.clone())?;
        let url = preview.url_with(forward.port, true);
        forwards.insert(address, forward);
        Ok(url)
    }

    pub fn active(&self, session: Option<&str>, port: u16) -> bool {
        self.forwards.lock().unwrap().iter().any(|(address, forward)| {
            address.port() == port
                && match &forward.source {
                    PreviewSource::Session(id) => session == Some(id.as_str()),
                    PreviewSource::Machine(_) => session.is_none(),
                }
        })
    }

    pub fn stop(&self, port: u16) {
        self.forwards.lock().unwrap().retain(|address, _| address.port() != port);
    }
}

struct Forward {
    port: u16,
    source: PreviewSource,
    stop: UnixStream,
    thread: Option<JoinHandle<()>>,
}

impl Forward {
    fn start(target: SocketAddr, source: PreviewSource) -> io::Result<Self> {
        let (port, sockets) = super::bind_loopbacks(0, true)?;
        let listeners = sockets.into_iter().map(Async::new).collect::<io::Result<Vec<_>>>()?;
        let (lifetime, stop) = UnixStream::pair()?;
        let mut stop = Async::new(stop)?;
        let thread = std::thread::Builder::new().name("https-preview".into()).spawn(move || {
            let incoming = stream::select_all(listeners.iter().map(|socket| Box::pin(socket.incoming())));
            let serve = incoming.try_for_each_concurrent(MAX_TUNNELS, |browser| async move {
                if let Ok(service) = Async::<TcpStream>::connect(target).await {
                    let _ = future::try_join(copy(&browser, &service), copy(&service, &browser)).await;
                }
                Ok(())
            });
            let mut byte = [0];
            async_io::block_on(future::select(Box::pin(serve), Box::pin(stop.read(&mut byte))));
        })?;
        Ok(Self { port, source, stop: lifetime, thread: Some(thread) })
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        // EOF cancels both accepts and all in-flight copies before the port is released.
        let _ = self.stop.shutdown(Shutdown::Write);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn copy(from: &Async<TcpStream>, mut to: &Async<TcpStream>) -> io::Result<()> {
    futures_util::io::copy(from, &mut to).await?;
    to.get_ref().shutdown(Shutdown::Write)
}
