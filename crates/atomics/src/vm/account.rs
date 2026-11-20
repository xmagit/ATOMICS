//! Authenticate a generation before giving its broker private input. This also
//! works with older images: the guest needs only sshd, sudo and its system Python.
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};

use super::BundleInfo;

const TIMEOUT: Duration = Duration::from_secs(30);
const HELPER: &str = include_str!("account.py");
const TLS_HELPER: &str = concat!(include_str!("localhost_tls.py"), "\n", include_str!("account.py"));

#[derive(Clone, Copy)]
pub(crate) enum Operation {
    Probe,
    Broker,
    LocalhostTls,
}

pub(crate) fn command(info: &BundleInfo, local: &Path, operation: Operation) -> Command {
    let remote = format!("/run/atomics-account-{}", uuid::Uuid::new_v4());
    let login = info.admin.as_ref().unwrap_or(&info.user);
    let quote = super::update::quote;
    let (helper, operation) = match operation {
        Operation::Probe => (HELPER, "probe"),
        Operation::Broker => (HELPER, "broker"),
        Operation::LocalhostTls => (TLS_HELPER, "localhost-tls"),
    };
    let script = format!(
        "exec /run/wrappers/bin/sudo -n /run/current-system/sw/bin/python3 -I -c {} {} {} {} {}",
        quote(helper),
        quote(&remote),
        quote(&info.user),
        quote(login),
        operation
    );
    let mut ssh = Command::new("ssh");
    ssh.arg("-F")
        .arg(super::lima_home().join(&info.instance).join("ssh.config"))
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
            "-o",
            "ExitOnForwardFailure=yes",
            "-T",
            "-L",
        ])
        .arg(format!("{}:{remote}", local.display()))
        .args(["-l", login])
        .arg(format!("lima-{}", info.instance))
        .arg(script);
    ssh
}

pub(crate) struct Connection {
    ssh: Child,
    stream: UnixStream,
    pub system: String,
    deadline: Instant,
    _dir: tempfile::TempDir,
}

fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| anyhow::anyhow!("VM admin connection timed out"))
}

/// Read one bounded handshake line. Partial replies share the original deadline.
fn handshake_line(mut stream: &UnixStream, deadline: Instant) -> Result<String> {
    let mut line = Vec::new();
    let mut buf = [0; 128];
    while line.len() < 512 {
        stream.set_read_timeout(Some(remaining(deadline)?))?;
        let n = stream.read(&mut buf)?;
        ensure!(n > 0, "VM closed the administrator handshake");
        if let Some(end) = buf[..n].iter().position(|&b| b == b'\n') {
            line.extend_from_slice(&buf[..end]);
            return Ok(String::from_utf8(line)?);
        }
        line.extend_from_slice(&buf[..n]);
    }
    anyhow::bail!("VM administrator handshake was too long")
}

impl Connection {
    pub fn open(info: &BundleInfo, operation: Operation) -> Result<Self> {
        Self::open_with(TIMEOUT, |local| Ok(command(info, local, operation)))
            .context("opening the VM administrator connection")
    }

    pub(crate) fn open_with(timeout: Duration, connect: impl FnOnce(&Path) -> Result<Command>) -> Result<Self> {
        // macOS's ordinary temporary-directory path can exceed sockaddr_un.sun_path.
        let dir = tempfile::Builder::new().permissions(std::fs::Permissions::from_mode(0o700)).tempdir_in("/tmp")?;
        let local = dir.path().join("account.sock");
        let (output, child_output) = UnixStream::pair()?;
        let deadline = Instant::now() + timeout;
        let mut ssh = connect(&local)?
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(Stdio::from(std::os::fd::OwnedFd::from(child_output)))
            .spawn()?;
        let open = || -> Result<(UnixStream, String)> {
            ensure!(
                handshake_line(&output, deadline)? == "atomics-account-ready",
                "VM administrator connection did not become ready"
            );
            let stream = UnixStream::connect(local)?;
            let system = handshake_line(&stream, deadline)?;
            ensure!(super::update::store_path(&system), "VM did not identify its running generation");
            Ok((stream, system))
        };
        match open() {
            Ok((stream, system)) => Ok(Self { ssh, stream, system, deadline, _dir: dir }),
            Err(error) => {
                let _ = ssh.kill();
                let _ = ssh.wait();
                Err(error)
            }
        }
    }

    pub fn send(self, bytes: &[u8]) -> Result<()> {
        self.exchange(bytes, 0).map(|_| ())
    }

    /// Close the request before reading any reply, under the original handshake deadline.
    pub fn exchange(mut self, mut bytes: &[u8], reply_limit: usize) -> Result<Vec<u8>> {
        while !bytes.is_empty() {
            self.stream.set_write_timeout(Some(remaining(self.deadline)?))?;
            let n = self.stream.write(bytes)?;
            ensure!(n > 0, "VM closed the administrator connection");
            bytes = &bytes[n..];
        }
        if reply_limit != 0 {
            // A peer with an empty reply can disconnect as soon as it sees EOF.
            // Darwin can reject changing a socket's timeout after that disconnect.
            self.stream.set_read_timeout(Some(remaining(self.deadline)?.min(Duration::from_millis(100))))?;
        }
        self.stream.shutdown(std::net::Shutdown::Write).context("closing the VM administrator request")?;
        let mut reply = Vec::new();
        if reply_limit != 0 {
            let mut buf = [0; 4096];
            loop {
                remaining(self.deadline)?;
                let n = match self.stream.read(&mut buf) {
                    Ok(n) => n,
                    Err(error)
                        if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
                    {
                        continue;
                    }
                    Err(error) => return Err(error).context("reading the administrator reply"),
                };
                if n == 0 {
                    break;
                }
                ensure!(reply.len() + n <= reply_limit, "VM administrator reply is too large");
                reply.extend_from_slice(&buf[..n]);
            }
        }
        loop {
            if let Some(status) = self.ssh.try_wait()? {
                ensure!(status.success(), "VM administrator command failed ({status})");
                return Ok(reply);
            }
            std::thread::sleep(Duration::from_millis(20).min(remaining(self.deadline)?));
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        let _ = self.ssh.kill();
        let _ = self.ssh.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_account_exchange_is_private_and_has_one_deadline() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vm-account.py");
        let record = tempfile::tempdir().unwrap();
        for fault in ["directory-mode", "no-generation", "invalid-generation", "slow-ready", "stall-after-write"] {
            let valid = "/nix/store/00000000000000000000000000000000-test";
            std::fs::write(
                record.path().join("running"),
                if fault == "invalid-generation" { "unknown" } else { valid },
            )
            .unwrap();
            let started = Instant::now();
            let connection = Connection::open_with(Duration::from_millis(500), |local| {
                if fault == "directory-mode" {
                    assert_eq!(local.parent().unwrap().metadata()?.permissions().mode() & 0o777, 0o700);
                }
                let mut cmd = Command::new("python3");
                cmd.arg(&fixture)
                    .args([
                        "-L",
                        &format!("{}:/run/atomics-account-test", local.display()),
                        "ControlMaster=no",
                        "ControlPath=none",
                        "/run/wrappers/bin/sudo -n",
                    ])
                    .env("RECORD", record.path())
                    .env("ACCOUNT_FAULT", fault);
                Ok(cmd)
            });
            if fault == "directory-mode" {
                drop(connection.unwrap());
                continue;
            }
            let result = connection.and_then(|c| c.send(b"test mutation"));
            assert!(result.is_err(), "{fault} unexpectedly succeeded");
            assert!(started.elapsed() < Duration::from_secs(2), "{fault} exceeded the deadline");
            if fault != "stall-after-write" {
                assert!(!record.path().join("stdin").exists(), "{fault} must fail before the write");
            }
        }
    }
}
