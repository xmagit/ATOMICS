//! `atomics --connect vm` (`[client] connect = "vm"`): the window as a client of the ATOMICS host in this
//! Mac's VM (docs/nixos-vm.md). Each dial makes sure of what that needs, in order: the VM is running
//! (only the Start button starts it), the bundle's forward to the guest's port is up
//! (`bin/atomics-vm-host-forward`, held as a child of this window), and this window is paired with the
//! host. Pairing happens once: the host mints a token over Lima's ssh (`atomics ctl pair_client` in the
//! guest), and the token is kept in this Mac's keychain. A token the host refuses is kept until the user
//! says to pair again (`client.rs`).
//!
//! The forward listens on a Unix socket in a folder this window made and only this user can enter, and
//! the token is sent there alone: on a loopback port, whatever listened first would get it, another
//! account's program or another forward among them.

use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use zeroize::Zeroizing;

use super::link::{Address, Endpoint};
use crate::relay::Keychain;
use crate::vm::{BundleInfo, Status};

/// The keychain item that holds this Mac's token for a VM's host, by instance.
pub const KEYCHAIN_SERVICE: &str = "atomics-vm-host-v1";

/// What the window shows of the VM while it is not connected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub instance: String,
    pub status: Status,
    /// What the last dial was doing or why it stopped.
    pub note: Option<String>,
    /// A start is under way.
    pub starting: bool,
}

pub struct Gate {
    info: Option<BundleInfo>,
    install_dir: PathBuf,
    keychain: Keychain,
    shared: Arc<Mutex<Shared>>,
}

#[derive(Default)]
struct Shared {
    status: Option<Status>,
    note: Option<String>,
    starting: bool,
    forward: Option<Forward>,
}

/// The bundle's forward, a child of this window, listening on `socket` in `folder`: a folder made for
/// it alone, which only this user can enter. Ending it removes both.
struct Forward {
    child: Child,
    folder: tempfile::TempDir,
    socket: PathBuf,
}

impl Drop for Forward {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Gate {
    pub fn new() -> Self {
        let (install_dir, info) = match BundleInfo::installed() {
            Some((dir, info)) => (dir, Some(info)),
            None => (crate::vm::installed_dir(), None),
        };
        Self { info, install_dir, keychain: Keychain::current(), shared: Arc::default() }
    }

    /// A gate for the bundle `info` in `install_dir`, its token in `keychain`.
    #[cfg(test)]
    pub(crate) fn with(info: Option<BundleInfo>, install_dir: PathBuf, keychain: Keychain) -> Self {
        Self { info, install_dir, keychain, shared: Arc::default() }
    }

    pub fn instance(&self) -> Option<&str> {
        self.info.as_ref().map(|info| info.instance.as_str())
    }

    pub fn bundle(&self) -> Option<BundleInfo> {
        self.info.clone()
    }

    pub fn view(&self) -> View {
        let shared = self.shared.lock().unwrap();
        View {
            instance: self.instance().unwrap_or("atomics").to_string(),
            status: shared.status.clone().unwrap_or(Status::Unknown("checking".into())),
            note: shared.note.clone(),
            starting: shared.starting,
        }
    }

    /// Start the VM on a thread of its own (through the QA queue); `done` is called when it is up or failed.
    pub fn start(&self, done: Arc<dyn Fn() + Send + Sync>) {
        let Some(info) = self.info.clone() else { return };
        {
            let mut shared = self.shared.lock().unwrap();
            if shared.starting {
                return;
            }
            shared.starting = true;
            shared.status = Some(Status::Starting);
            shared.note = Some(format!("Starting the VM {}…", info.instance));
        }
        let shared = self.shared.clone();
        let _ = std::thread::Builder::new().name("atomics-vm-start".into()).spawn(move || {
            let result = crate::vm::start(&info.instance);
            {
                let mut shared = shared.lock().unwrap();
                shared.starting = false;
                shared.note = result.err().map(|e| format!("{e:#}"));
                shared.status = Some(crate::vm::status(&info.instance, false));
            }
            done();
        });
    }

    /// A dial's resolver: everything the host needs, then its endpoint. Runs on the link's thread.
    pub fn resolver(&self) -> super::Resolve {
        let info = self.info.clone();
        let install_dir = self.install_dir.clone();
        let keychain = self.keychain.clone();
        let shared = self.shared.clone();
        Box::new(move || {
            let result = resolve(info, &install_dir, &shared, &keychain);
            if let Err(error) = &result {
                // A VM that is not running or not set up says so itself; what failed past that is noted.
                let mut shared = shared.lock().unwrap();
                if shared.status == Some(Status::Running) {
                    shared.note = Some(format!("{error:#}"));
                }
            }
            result
        })
    }

    /// The user said to pair again after the host refused the token: forget it, so the next dial pairs.
    pub fn forget_token(&self) -> Result<()> {
        match &self.info {
            Some(info) => self.keychain.remove_item(KEYCHAIN_SERVICE, &info.instance),
            None => Ok(()),
        }
    }
}

fn set(shared: &Mutex<Shared>, status: Status, note: Option<String>) {
    let mut shared = shared.lock().unwrap();
    if !shared.starting {
        shared.status = Some(status);
    }
    shared.note = note;
}

fn resolve(
    info: Option<BundleInfo>,
    install_dir: &Path,
    shared: &Mutex<Shared>,
    keychain: &Keychain,
) -> Result<Endpoint> {
    let Some(info) = info else {
        set(shared, Status::NotSetUp, None);
        bail!(
            "no ATOMICS VM is set up on this Mac: build it with scripts/macos-vm-build.sh --host and run atomics vm setup"
        );
    };
    if info.host.is_none() {
        set(shared, Status::NotSetUp, None);
        bail!("the VM's bundle has no ATOMICS host: rebuild it with scripts/macos-vm-build.sh --host");
    }
    let status = crate::vm::status(&info.instance, false);
    let running = status == Status::Running;
    set(shared, status.clone(), None);
    if !running {
        bail!("the VM {} is {}", info.instance, status.label());
    }
    reach(&info, install_dir, shared, keychain)
}

/// The running VM's host: this window's forward to it, and the token to give it there.
fn reach(info: &BundleInfo, install_dir: &Path, shared: &Mutex<Shared>, keychain: &Keychain) -> Result<Endpoint> {
    let port = info.host.as_ref().map(|host| host.port).context("the VM's bundle has no ATOMICS host")?;
    let socket = forward(info, install_dir, shared)?;
    let token = token(info, keychain)?;
    // The host sees a client on its own loopback, whatever this end of the forward is.
    Ok(Endpoint { addr: Address::Unix(socket), host: format!("127.0.0.1:{port}"), token })
}

/// The bundle's forward, started unless it runs, and answering on its socket: that socket.
fn forward(info: &BundleInfo, install_dir: &Path, shared: &Mutex<Shared>) -> Result<PathBuf> {
    let socket = {
        let mut shared = shared.lock().unwrap();
        let alive = shared.forward.as_mut().is_some_and(|forward| matches!(forward.child.try_wait(), Ok(None)));
        if !alive {
            // An ended forward's folder goes with it; the new one gets a folder of its own.
            shared.forward = None;
            shared.forward = Some(Forward::start(&install_dir.join("bin/atomics-vm-host-forward"))?);
        }
        let forward = shared.forward.as_ref().expect("started above");
        private_folder(forward.folder.path())?;
        forward.socket.clone()
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if answers(&socket) {
            return Ok(socket);
        }
        let mut shared = shared.lock().unwrap();
        if let Some(forward) = shared.forward.as_mut()
            && let Ok(Some(status)) = forward.child.try_wait()
        {
            let mut why = String::new();
            if let Some(mut stderr) = forward.child.stderr.take() {
                use std::io::Read;
                let _ = stderr.read_to_string(&mut why);
            }
            shared.forward = None;
            // 64 is a usage error: a bundle from before forwards to a socket takes a port only.
            let hint = if status.code() == Some(64) {
                " (rebuild the bundle with scripts/macos-vm-build.sh --host and run atomics vm setup)"
            } else {
                ""
            };
            bail!("the forward to the VM {} ended: {}{hint}", info.instance, why.trim());
        }
        drop(shared);
        if Instant::now() > deadline {
            bail!("the forward to the VM {} does not answer on {}", info.instance, socket.display());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

impl Forward {
    /// `script` with a socket in a new private folder, as a child of this window.
    fn start(script: &Path) -> Result<Forward> {
        let folder = private_tempdir()?;
        let socket = folder.path().join("host.sock");
        let child = Command::new("/bin/sh")
            .arg(script)
            .arg(&socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("starting {}", script.display()))?;
        Ok(Forward { child, folder, socket })
    }
}

/// A folder made now (never one that was already there) that only this user can enter, for one
/// forward's socket. In the user's temporary folder, or `/tmp` when that path is too long for a socket.
fn private_tempdir() -> Result<tempfile::TempDir> {
    let mut base = std::env::temp_dir();
    // A socket's path holds 104 bytes on macOS; the folder and the socket's name add 30.
    if base.as_os_str().len() > 64 {
        base = PathBuf::from("/tmp");
    }
    let folder = tempfile::Builder::new()
        .prefix("atomics-vm-")
        .tempdir_in(&base)
        .with_context(|| format!("making a private folder in {}", base.display()))?;
    std::fs::set_permissions(folder.path(), std::fs::Permissions::from_mode(0o700))?;
    private_folder(folder.path())?;
    Ok(folder)
}

/// `folder` is a folder (not a link to one) of this user's that no one else may enter.
fn private_folder(folder: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(folder).with_context(|| format!("{} is gone", folder.display()))?;
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        bail!("{} is not a folder only this user can enter", folder.display());
    }
    Ok(())
}

/// A socket of this user's at `socket` takes a connection.
fn answers(socket: &Path) -> bool {
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let ours = std::fs::symlink_metadata(socket).is_ok_and(|meta| meta.file_type().is_socket() && meta.uid() == uid);
    ours && UnixStream::connect(socket).is_ok()
}

/// The token from the keychain, or a new one the host mints for this Mac, kept there.
fn token(info: &BundleInfo, keychain: &Keychain) -> Result<Zeroizing<String>> {
    if let Some(token) = keychain.read_item(KEYCHAIN_SERVICE, &info.instance)?.filter(|t| !t.is_empty()) {
        return Ok(Zeroizing::new(token));
    }
    let name = format!("ATOMICS on {}", atomics_core::paths::host_name());
    let request = serde_json::json!({ "cmd": "pair_client", "name": name }).to_string();
    // A login shell in the guest, so `atomics ctl` reaches the host's own socket (ATOMICS_SOCKET).
    let output = crate::vm::guest_shell(info)?
        .args(["bash", "--login", "-c", r#"exec atomics ctl "$1""#, "atomics-pair", &request])
        .stderr(Stdio::piped())
        .output()
        .context("asking the VM's host to pair this Mac")?;
    let answer: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_default();
    let Some(token) = answer["result"]["token"].as_str().filter(|_| output.status.success()) else {
        bail!(
            "the VM's host did not pair this Mac: {}",
            answer["error"]
                .as_str()
                .map_or_else(|| String::from_utf8_lossy(&output.stderr).trim().to_string(), str::to_string)
        );
    };
    let token = Zeroizing::new(token.to_string());
    let label = format!("ATOMICS host in the VM {}", info.instance);
    keychain.write_item(KEYCHAIN_SERVICE, &info.instance, &label, &token)?;
    Ok(token)
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::os::unix::net::UnixListener;

    use super::*;
    use crate::vm::HostInfo;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn bundle(dir: &Path, port: u16, script: &str) -> BundleInfo {
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let path = dir.join("bin/atomics-vm-host-forward");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        BundleInfo {
            version: 1,
            instance: "atomics-test".into(),
            user: "atomics".into(),
            home: "/home/atomics".into(),
            mac_user: "user".into(),
            install_dir: dir.display().to_string(),
            host: Some(HostInfo { port }),
            admin: None,
            build: None,
        }
    }

    fn keychain_with_token(dir: &Path, info: &BundleInfo) -> Keychain {
        let keychain = Keychain::in_folder(&dir.join("credentials"));
        keychain.write_item(KEYCHAIN_SERVICE, &info.instance, "test", TOKEN).unwrap();
        keychain
    }

    /// Another program on the bundle's port, as another account (or another forward) could have: the
    /// window never dials it. Its own forward's socket, in a folder only this user can enter, is where
    /// the token goes.
    #[test]
    fn a_listener_on_the_port_never_gets_the_token() {
        let dir = crate::test_support::tempdir().unwrap();
        let foreign = TcpListener::bind("127.0.0.1:0").unwrap();
        foreign.set_nonblocking(true).unwrap();
        let port = foreign.local_addr().unwrap().port();
        // The stand-in forward says where it was asked to listen, and the test listens there as ssh would.
        let asked = dir.path().join("asked");
        let script =
            format!("#!/bin/sh\nprintf '%s' \"$1\" > {}.part && mv {0}.part {0}\nexec sleep 60\n", asked.display());
        let info = bundle(dir.path(), port, &script);
        let keychain = keychain_with_token(dir.path(), &info);
        let tokens = Arc::new(Mutex::new(Vec::<String>::new()));
        let host = {
            let (asked, tokens) = (asked.clone(), tokens.clone());
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(10);
                while !asked.exists() {
                    assert!(Instant::now() < deadline, "the forward was never started");
                    std::thread::sleep(Duration::from_millis(10));
                }
                let socket = PathBuf::from(std::fs::read_to_string(&asked).unwrap());
                let listener = UnixListener::bind(&socket).unwrap();
                // The forward's readiness check, then the client's upgrade.
                for stream in listener.incoming().flatten() {
                    let mut request = String::new();
                    let mut reader = BufReader::new(&stream);
                    while reader.read_line(&mut request).is_ok_and(|n| n > 2) {}
                    let bearer = request.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("authorization").then(|| value.trim().strip_prefix("Bearer "))?
                    });
                    let Some(bearer) = bearer else { continue };
                    tokens.lock().unwrap().push(bearer.to_string());
                    let _ = (&stream).write_all(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n");
                    return socket;
                }
                unreachable!("the listener ended")
            })
        };
        let shared = Mutex::new(Shared::default());
        let endpoint = reach(&info, dir.path(), &shared, &keychain).unwrap();
        let Address::Unix(socket) = &endpoint.addr else { panic!("the forward is dialled on a port: {endpoint:?}") };
        let folder = socket.parent().unwrap();
        let meta = std::fs::symlink_metadata(folder).unwrap();
        assert_eq!(meta.mode() & 0o777, 0o700, "{}", folder.display());
        assert!(!folder.starts_with(dir.path()), "a folder of the forward's own");
        assert_eq!(endpoint.host, format!("127.0.0.1:{port}"));
        let refused = super::super::link::connect(&endpoint).map(|_| ()).unwrap_err();
        assert_eq!(refused.code, atomics_core::tunnel::close::UNAUTHORISED);
        assert_eq!(&host.join().unwrap(), socket);
        assert_eq!(*tokens.lock().unwrap(), [TOKEN], "the token went to the forward's socket");
        assert!(foreign.accept().is_err_and(|e| e.kind() == std::io::ErrorKind::WouldBlock), "the port was dialled");
        // Ending the forward takes its folder with it.
        let folder = folder.to_path_buf();
        drop(shared);
        assert!(!folder.exists(), "{}", folder.display());
    }

    #[test]
    fn the_forwards_folder_is_this_users_alone() {
        let folder = private_tempdir().unwrap();
        assert_eq!(std::fs::metadata(folder.path()).unwrap().mode() & 0o777, 0o700);
        assert!(folder.path().join("host.sock").as_os_str().len() < 104, "too long for a socket");
        private_folder(folder.path()).unwrap();
        std::fs::set_permissions(folder.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_folder(folder.path()).is_err(), "others may enter");
        let dir = crate::test_support::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join("link")).unwrap();
        assert!(private_folder(&dir.path().join("link")).is_err(), "a link");
        // A socket someone else made, or a plain file, is not the forward.
        assert!(!answers(&real.join("absent.sock")));
        std::fs::write(real.join("file"), "").unwrap();
        assert!(!answers(&real.join("file")));
    }

    #[test]
    fn a_forward_that_ends_says_why() {
        let dir = crate::test_support::tempdir().unwrap();
        let script = "#!/bin/sh\necho \"atomics-vm-host-forward: $1 is not a port\" >&2\nexit 64\n";
        let info = bundle(dir.path(), 47800, script);
        let shared = Mutex::new(Shared::default());
        let error = format!("{:#}", forward(&info, dir.path(), &shared).unwrap_err());
        assert!(error.contains("is not a port") && error.contains("rebuild the bundle"), "{error}");
        assert!(shared.lock().unwrap().forward.is_none());
    }
}
