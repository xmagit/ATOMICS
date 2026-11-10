//! `atomics host`: the runtime (`host::Host`) and its local socket with no window, egui, display
//! or GPU. Hooks, `atomics ctl` and MCP keep working through the socket with no client attached;
//! the `atomics.v1` protocol thread runs when `[remote] enabled = true`, exactly as in the desktop.
//!
//! Made for a service manager: it stays in the foreground, logs to stderr, and SIGTERM (or SIGINT,
//! or SIGHUP unless it is ignored) stops it cleanly. A second signal while stopping ends it at once.
//! It fails (exit 1) rather than run half-reachable: without the local socket, or with `[remote]`
//! enabled and a protocol server that cannot start.

use std::io::Read;
use std::os::fd::IntoRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, Ordering};

use atomics_core::paths;

use crate::app::{Options, State};
use crate::instance::Lock;

/// Runs until a stop signal; returns the process exit code.
pub fn run(options: Options) -> i32 {
    let home = paths::atomics_home();
    let _lock = match Lock::host(&home) {
        Ok(lock) => lock,
        Err(refused) => {
            log(&refused.to_string());
            return 1;
        }
    };
    let signals = match Signals::install() {
        Ok(signals) => signals,
        Err(error) => {
            log(&format!("cannot handle stop signals: {error}"));
            return 1;
        }
    };
    crate::crash::install(&home);
    // This host owns the home, so an advert an earlier run left names a server that is gone. With
    // `[remote]` on, the protocol server writes a fresh one as it starts.
    let advert = home.join("protocol.json");
    let _ = std::fs::remove_file(&advert);
    let socket = paths::socket_path();
    let state = State::build(options, crate::host::Window::default());
    if let Some(error) = state.hook_error() {
        // Without the socket nothing could reach this host: no hooks, ctl or MCP.
        log(&format!("the local socket is unavailable: {error}"));
        return 1;
    }
    let remote = state.remote_access();
    let host = state.into_host();
    log(&format!(
        "started: pid {}, home {}, socket {}, config {}",
        std::process::id(),
        home.display(),
        socket.display(),
        paths::config_file().display()
    ));
    // The URL only: the token is in protocol.json (mode 0600) and never in a log.
    match (host.protocol_addr(), host.protocol_error()) {
        (Some(addr), _) => {
            log(&format!("atomics.v1 protocol on ws://{addr}; clients read the token from {}", advert.display()))
        }
        (None, Some(error)) => {
            // A host clients cannot reach is not healthy: say why and let the service manager see it.
            log(&format!("[remote] is enabled but the protocol server cannot start (port {}): {error}", remote.port));
            stop(host);
            return 1;
        }
        (None, None) => log("protocol server off ([remote] enabled = false)"),
    }
    // Its progress (registered, online, channels) is in `atomics ctl '{"cmd":"remote"}'`; a pass never is.
    match host.rendezvous_status() {
        Some(status) if status.phase == crate::protocol::rendezvous::Phase::Stopped => {
            log(&format!("rendezvous enabled but not started: {}", status.note.unwrap_or_default()))
        }
        Some(_) => log("rendezvous on; atomics ctl '{\"cmd\":\"remote\"}' shows its status"),
        None => {}
    }
    let signal = signals.wait();
    log(&format!("{} received; stopping", signal_name(signal)));
    stop(host);
    0
}

/// No new hooks, ctl or MCP requests (the socket goes first; its listener thread ends with the
/// process), then the protocol server and the runtime, then the shutdown the desktop does on quit.
fn stop(mut host: crate::host::Host) {
    host.state().stop_accepting_control();
    host.stop();
    host.state().shutdown();
    drop(host);
    log("stopped");
}

fn log(message: &str) {
    eprintln!("atomics host: {message}");
}

fn signal_name(signal: i32) -> &'static str {
    match signal {
        libc::SIGTERM => "SIGTERM",
        libc::SIGINT => "SIGINT",
        libc::SIGHUP => "SIGHUP",
        _ => "a signal",
    }
}

/// The write end of the self-pipe the handler signals through, or -1.
static PIPE: AtomicI32 = AtomicI32::new(-1);
static RECEIVED: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(signal: libc::c_int) {
    // Only async-signal-safe work: an atomic store and a non-blocking write.
    RECEIVED.store(signal, Ordering::SeqCst);
    let fd = PIPE.load(Ordering::SeqCst);
    if fd >= 0 {
        // SAFETY: write(2) is async-signal-safe; the buffer outlives the call.
        unsafe { libc::write(fd, [1u8].as_ptr().cast(), 1) };
    }
}

/// Stop signals, delivered to whichever thread they hit and read on the main thread. Handlers
/// (not a blocked signal mask) are used so that agents never inherit a changed mask; exec resets
/// caught signals to their defaults in every child.
struct Signals {
    read: UnixStream,
    handled: Vec<libc::c_int>,
}

impl Signals {
    fn install() -> std::io::Result<Self> {
        let (read, write) = UnixStream::pair()?;
        write.set_nonblocking(true)?;
        // Leaked on purpose: the handler may write to it until the process ends.
        PIPE.store(write.into_raw_fd(), Ordering::SeqCst);
        let mut handled = vec![libc::SIGTERM, libc::SIGINT];
        if !ignored(libc::SIGHUP)? {
            handled.push(libc::SIGHUP);
        }
        for &signal in &handled {
            set_handler(signal, on_signal as *const () as libc::sighandler_t)?;
        }
        Ok(Self { read, handled })
    }

    /// Blocks until a stop signal arrives and returns it. A second one then takes the default
    /// action, so a stuck shutdown can still be interrupted.
    fn wait(&self) -> i32 {
        let mut byte = [0u8; 1];
        loop {
            match (&self.read).read(&mut byte) {
                Ok(1) => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                // The write end is never closed; anything else is unexpected, so stop too.
                _ => break,
            }
        }
        for &signal in &self.handled {
            let _ = set_handler(signal, libc::SIG_DFL);
        }
        RECEIVED.load(Ordering::SeqCst)
    }
}

fn ignored(signal: libc::c_int) -> std::io::Result<bool> {
    // SAFETY: a zeroed sigaction is a valid out-parameter; a null new action only queries.
    let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
    if unsafe { libc::sigaction(signal, std::ptr::null(), &mut current) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(current.sa_sigaction == libc::SIG_IGN)
}

fn set_handler(signal: libc::c_int, handler: libc::sighandler_t) -> std::io::Result<()> {
    // SAFETY: the action is fully initialised; `on_signal` is async-signal-safe.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}
