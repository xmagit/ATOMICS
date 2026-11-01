//! The sign-in experience around `mcp_client::OAuth`: what state a ticket server is in, a sign-in or
//! sign-out running on a thread of its own with a cancel, headless detection, the doctor line, and
//! `atomics tickets login|logout`.
//!
//! Needing a sign-in is a state the person can act on, not a failure to back off from. Nothing here
//! opens a browser except a sign-in the person started, and nothing prints a token, a client id or an
//! endpoint: only the server's host and whether an entry is present.

use std::io::BufRead;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use atomics_core::tickets::{McpServer, TicketConfig};

use crate::mcp_client::{OAuth, Revocation, SignInControl, SignInEvent, SignInStatus};

/// Where one ticket server stands. `SigningIn` is held by whoever runs the sign-in job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignState {
    /// Nothing saved, or the server refused what was: the Sign in banner shows.
    SignedOut,
    SigningIn,
    SignedIn,
    /// The keychain could not be read; free of any stored value.
    Unavailable(String),
}

impl SignState {
    /// From what the keychain holds. It never touches the network.
    pub fn of(oauth: &OAuth, server: &McpServer) -> SignState {
        if !server.uses_oauth() {
            return SignState::SignedIn;
        }
        match oauth.status(server) {
            SignInStatus::SignedIn => SignState::SignedIn,
            SignInStatus::NeedsSignIn => SignState::SignedOut,
            SignInStatus::Error(message) => SignState::Unavailable(message),
        }
    }
}

/// Whether this machine has no browser to open: Linux without a display, or a login over SSH.
pub fn headless() -> bool {
    headless_in(std::env::consts::OS, |name| std::env::var_os(name).is_some_and(|v| !v.is_empty()))
}

pub(crate) fn headless_in(os: &str, set: impl Fn(&str) -> bool) -> bool {
    set("SSH_CONNECTION") || set("SSH_TTY") || (os == "linux" && !set("DISPLAY") && !set("WAYLAND_DISPLAY"))
}

/// A server's host for display, never its path or query.
pub fn host_of(server: &McpServer) -> String {
    server
        .url
        .as_deref()
        .and_then(|url| url::Url::parse(url.trim()).ok())
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| "the server".into())
}

/// What the person sees while a sign-in waits.
#[derive(Default)]
pub struct Progress {
    /// The authorization address: for the browser here, or any other machine's.
    pub url: Option<String>,
    pub browser_failed: Option<String>,
    pub rejected: Option<String>,
}

enum Message {
    Event(SignInEvent),
    Finished(Result<(), String>),
}

/// A sign-in running on its own thread.
pub struct SignInJob {
    cancel: Arc<AtomicBool>,
    paste: mpsc::Sender<String>,
    messages: Receiver<Message>,
    pub started: Instant,
    pub progress: Progress,
    /// The longest the sign-in waits for the browser.
    pub patience: Duration,
}

impl SignInJob {
    /// Starts signing in. `open_browser` is off on a headless machine. `wake` repaints the window.
    pub fn start(oauth: OAuth, server: McpServer, open_browser: bool, wake: &crate::host::Wake) -> SignInJob {
        let (tx, messages) = wake.channel::<Message>();
        let (paste, pasted) = mpsc::channel::<String>();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        std::thread::Builder::new()
            .name("atomics-sign-in".into())
            .spawn(move || {
                let notify = |event| {
                    let _ = tx.send(Message::Event(event));
                };
                let control = SignInControl {
                    cancel: &flag,
                    paste: &pasted,
                    notify: &notify,
                    open_browser,
                    can_paste: true,
                };
                let result = oauth.sign_in_with(&server, Duration::from_secs(30), &control).map_err(|e| e.to_string());
                let _ = tx.send(Message::Finished(result));
            })
            .expect("a thread can start");
        SignInJob {
            cancel,
            paste,
            messages,
            started: Instant::now(),
            progress: Progress::default(),
            patience: Duration::from_secs(300),
        }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Hands the sign-in the address a browser ended on.
    pub fn paste(&self, text: &str) {
        let _ = self.paste.send(text.to_string());
    }

    /// Takes what happened since the last call; `Some` once the sign-in is over.
    pub fn poll(&mut self) -> Option<Result<(), String>> {
        while let Ok(message) = self.messages.try_recv() {
            match message {
                Message::Event(SignInEvent::Authorize(url)) => self.progress.url = Some(url),
                Message::Event(SignInEvent::BrowserFailed(why)) => self.progress.browser_failed = Some(why),
                Message::Event(SignInEvent::PasteRejected(why)) => self.progress.rejected = Some(why),
                Message::Finished(result) => return Some(result),
            }
        }
        None
    }
}

/// Signs out on a thread: the revocation may wait on the network. The local sign-out always completes.
pub fn sign_out(oauth: OAuth, server: McpServer, wake: &crate::host::Wake) -> Receiver<Result<Revocation, String>> {
    let (tx, rx) = wake.channel();
    std::thread::Builder::new()
        .name("atomics-sign-out".into())
        .spawn(move || {
            let _ = tx.send(oauth.sign_out(&server).map_err(|e| e.to_string()));
        })
        .expect("a thread can start");
    rx
}

/// What a sign-out says to the person.
pub fn sign_out_message(host: &str, result: &Result<Revocation, String>) -> String {
    match result {
        Ok(Revocation::Revoked) => format!("Signed out of {host}; the server revoked the sign-in"),
        Ok(Revocation::NotOffered) => format!("Signed out of {host}"),
        Ok(Revocation::Failed(why)) => format!("Signed out of {host} here; revocation failed: {why}"),
        Err(why) => format!("Sign-out of {host} failed: {why}"),
    }
}

/// The doctor line for a server that signs in with OAuth; values hidden.
pub fn doctor_line(oauth: &OAuth, config: &TicketConfig) -> Option<String> {
    if !config.server.uses_oauth() {
        return None;
    }
    let host = host_of(&config.server);
    Some(match SignState::of(oauth, &config.server) {
        SignState::SignedIn => {
            format!("✓ credential MCP sign-in ({host}): present; OS keychain → ATOMICS → that server only (values hidden)")
        }
        SignState::Unavailable(why) => format!("✗ credential MCP sign-in ({host}) (warning): {why}"),
        _ => format!(
            "· credential MCP sign-in ({host}): absent; press Sign in in the Tickets panel or run `atomics tickets login`"
        ),
    })
}

/// The doctor line for `[tickets.remote]`.
pub fn remote_line(status: &atomics_core::tickets::RemoteStatus) -> String {
    use atomics_core::tickets::RemoteStatus;
    match status {
        RemoteStatus::On(owners) => format!("✓ remote control: on; steering humans {}", owners.join(", ")),
        RemoteStatus::Off(why) => format!("· remote control: off; {why}"),
        RemoteStatus::Error(why) => {
            format!("✗ remote control (error): {why} — fix [tickets.remote] owners or the connection's identity")
        }
    }
}

pub fn remote_failed_line(error: &str) -> String {
    format!("✗ remote control check failed (warning): whoami: {error} — check operations.whoami")
}

/// `atomics tickets login|logout`. Returns the exit code.
pub fn run_cli(login: bool, workspace: Option<&str>) -> i32 {
    let config = match crate::doctor::ticket_config_for(workspace) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("atomics tickets: {error:#}");
            return 1;
        }
    };
    if !config.is_configured() || !config.server.uses_oauth() {
        eprintln!("atomics tickets: [tickets.server] needs a url and oauth = true");
        return 1;
    }
    let oauth = OAuth::current();
    let host = host_of(&config.server);
    let wake = crate::host::Wake::default();
    if !login {
        let result = sign_out(oauth, config.server.clone(), &wake).recv().map_err(|e| e.to_string()).and_then(|r| r);
        println!("{}", sign_out_message(&host, &result));
        return i32::from(result.is_err());
    }
    let headless = headless();
    let mut job = SignInJob::start(oauth, config.server.clone(), !headless, &wake);
    let paste = job.paste.clone();
    if headless {
        // Pasting is read from the terminal; the thread ends with the process.
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines().map_while(Result::ok) {
                if !line.trim().is_empty() {
                    let _ = paste.send(line);
                }
            }
        });
    }
    let (mut shown, mut rejected) = (false, None::<String>);
    loop {
        if let Some(result) = job.poll() {
            return match result {
                Ok(()) => {
                    println!("Signed in to {host}.");
                    0
                }
                Err(error) => {
                    eprintln!("Sign-in to {host} failed: {error}");
                    1
                }
            };
        }
        if !shown && let Some(url) = job.progress.url.clone() {
            shown = true;
            if headless {
                println!("No browser here. Open this address on any machine and sign in:\n  {url}");
                println!("Then paste the address the browser ends on (http://127.0.0.1:…/callback?code=…):");
            } else {
                println!("Opening your browser. If it does not open, use this address:\n  {url}");
            }
        }
        if job.progress.rejected != rejected {
            rejected = job.progress.rejected.clone();
            if let Some(why) = &rejected {
                eprintln!("Not used: {why}");
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests;
