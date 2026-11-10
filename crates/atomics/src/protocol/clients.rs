//! Clients paired with this host by a token: a desktop on another machine that reaches the protocol
//! server through a forward it holds, such as ATOMICS on a Mac reaching the host in its VM through
//! an ssh forward (docs/nixos-vm.md). The per-start token in `protocol.json` is for clients on this
//! machine; a paired client keeps its token across restarts, in its own keychain.
//!
//! Pairing is `atomics ctl '{"cmd":"pair_client","name":…}'` on the host's private socket, so only
//! this user's processes on this machine can pair, which is who could read `protocol.json` anyway.
//! The answer carries the token once. The host keeps a SHA-256 digest of it in
//! `paired-clients.json` (mode 0600), never the token; pairing a name again replaces its token, and
//! `unpair_client` forgets it. A paired client is a controller, and its name is the lease holder
//! other clients see.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ring::digest;
use serde::{Deserialize, Serialize};

use super::link::{Link, LinkError, Waker};
use super::session::PeerDevice;

/// The longest name a client may pair under: the lease holder label, shown in a pane's header.
pub const MAX_NAME_CHARS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paired {
    pub name: String,
    /// SHA-256 of the token, as hex.
    digest: String,
    /// Unix seconds.
    pub paired_at: u64,
}

pub struct PairedClients {
    path: PathBuf,
    clients: Mutex<Vec<Paired>>,
}

impl std::fmt::Debug for PairedClients {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairedClients").field("path", &self.path).finish_non_exhaustive()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn digest_of(token: &str) -> String {
    hex(digest::digest(&digest::SHA256, token.as_bytes()).as_ref())
}

/// A name a person can read in a pane's header: printable, trimmed and short.
fn clean_name(name: &str) -> Option<String> {
    let name: String = name.trim().chars().filter(|c| !c.is_control()).take(MAX_NAME_CHARS).collect();
    (!name.is_empty()).then_some(name)
}

impl PairedClients {
    /// The clients paired with the host whose file is `path`; none when it does not exist yet.
    pub fn open(path: PathBuf) -> io::Result<Self> {
        let clients = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(io::Error::other)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        Ok(Self { path, clients: Mutex::new(clients) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Paired>> {
        self.clients.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Pair a client as `name`, replacing the token of one paired under that name before, and return
    /// its token. Nothing is kept if the file cannot be written.
    pub fn pair(&self, name: &str) -> io::Result<(String, String)> {
        let name = clean_name(name).ok_or_else(|| io::Error::other("a client pairs under a name"))?;
        let token = super::random_hex(32)?;
        let paired_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let mut clients = self.lock();
        let mut next: Vec<Paired> = clients.iter().filter(|c| c.name != name).cloned().collect();
        next.push(Paired { name: name.clone(), digest: digest_of(&token), paired_at });
        write(&self.path, &next)?;
        *clients = next;
        Ok((name, token))
    }

    /// Forget the client paired as `name`; whether there was one.
    pub fn unpair(&self, name: &str) -> io::Result<bool> {
        let Some(name) = clean_name(name) else { return Ok(false) };
        let mut clients = self.lock();
        let next: Vec<Paired> = clients.iter().filter(|c| c.name != name).cloned().collect();
        if next.len() == clients.len() {
            return Ok(false);
        }
        write(&self.path, &next)?;
        *clients = next;
        Ok(true)
    }

    pub fn list(&self) -> Vec<Paired> {
        self.lock().clone()
    }

    fn contains(&self, device: &PeerDevice) -> bool {
        self.lock().iter().any(|client| device.id == format!("paired-{}", &client.digest[..16]))
    }

    /// The paired client `token` belongs to. Every digest is compared in full, so the time taken says
    /// nothing about which one came close.
    pub fn find(&self, token: &str) -> Option<PeerDevice> {
        if token.is_empty() {
            return None;
        }
        let presented = digest_of(token);
        let mut found = None;
        for client in self.lock().iter() {
            if super::same_secret(presented.as_bytes(), client.digest.as_bytes()) {
                found = Some(PeerDevice { name: client.name.clone(), id: format!("paired-{}", &client.digest[..16]) });
            }
        }
        found
    }
}

/// The file, readable by this user only, replaced whole.
fn write(path: &Path, clients: &[Paired]) -> io::Result<()> {
    atomics_core::store::write_private(path, &serde_json::to_vec_pretty(clients).map_err(io::Error::other)?)
}

/// Check the allowlist throughout a live session, including either side of a blocking read.
/// Unpairing or replacing a token closes all of its links by the next session tick, even idle ones.
pub(super) struct PairedLink<'a> {
    pub inner: &'a mut dyn Link,
    pub clients: &'a PairedClients,
    pub device: &'a PeerDevice,
}

impl Link for PairedLink<'_> {
    fn send(&mut self, frame: &str) -> Result<(), LinkError> {
        if !self.clients.contains(self.device) {
            return Err(LinkError::Closed);
        }
        self.inner.send(frame)
    }

    fn recv(&mut self, wait: Duration) -> Result<Option<String>, LinkError> {
        if !self.clients.contains(self.device) {
            return Err(LinkError::Closed);
        }
        let frame = self.inner.recv(wait)?;
        if !self.clients.contains(self.device) {
            return Err(LinkError::Closed);
        }
        Ok(frame)
    }

    fn close(&mut self, reason: &str) {
        self.inner.close(reason);
    }
    fn close_with(&mut self, code: u16, reason: &str) {
        self.inner.close_with(code, reason);
    }
    fn waker(&self) -> Option<Arc<Waker>> {
        self.inner.waker()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_p7_unpair_normalizes_names_as_pair_does() {
        let dir = tempfile::tempdir().unwrap();
        let clients = PairedClients::open(dir.path().join("clients.json")).unwrap();
        for name in ["  de\u{7}vice  ".to_string(), "device".repeat(20)] {
            let (_, token) = clients.pair(&name).unwrap();
            assert!(clients.unpair(&name).unwrap());
            assert!(clients.find(&token).is_none());
        }
    }

    #[test]
    fn a_paired_token_finds_its_client_and_the_file_holds_no_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("paired-clients.json");
        let clients = PairedClients::open(path.clone()).unwrap();
        let (name, token) = clients.pair("  Mac\u{7} ").unwrap();
        assert_eq!(name, "Mac");
        assert_eq!(token.len(), 64);
        assert_eq!(clients.find(&token).map(|d| d.name), Some("Mac".into()));
        assert_eq!(clients.find(&token[1..]), None);
        assert_eq!(clients.find(""), None);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(&token), "only a digest is kept");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);

        // Kept across a restart; pairing the name again replaces the old token.
        let reopened = PairedClients::open(path.clone()).unwrap();
        assert!(reopened.find(&token).is_some());
        let (_, second) = reopened.pair("Mac").unwrap();
        assert!(reopened.find(&token).is_none(), "the old token no longer works");
        assert!(reopened.find(&second).is_some());
        assert_eq!(reopened.list().len(), 1);

        assert!(reopened.unpair("Mac").unwrap());
        assert!(!reopened.unpair("Mac").unwrap());
        assert!(reopened.find(&second).is_none());
        assert!(reopened.pair(" \u{1b} ").is_err(), "a name is required");
    }
}
