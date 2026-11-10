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
use atomics_web::driver::{Status as LinkStatus, Stop};
use zeroize::Zeroizing;

use super::link::{Address, Endpoint};
use crate::relay::Keychain;
use crate::vm::{BundleInfo, Status};

/// The keychain item that holds this Mac's token for a VM's host, by instance.
pub const KEYCHAIN_SERVICE: &str = "atomics-vm-host-v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Vm(Status),
    HostDisconnected,
    Unauthorised,
    Stopping,
}

impl State {
    pub fn headline(&self, instance: &str) -> String {
        match self {
            Self::Vm(Status::NotSetUp) => "The ATOMICS VM is not set up on this Mac.".into(),
            Self::Vm(Status::Starting) => format!("Starting the VM {instance}…"),
            Self::Vm(status) => format!("The VM {instance} is {}.", status.label()),
            Self::HostDisconnected => format!("VM {instance} running, host not connected"),
            Self::Unauthorised => format!("VM {instance} running, host refused this Mac's token"),
            Self::Stopping => format!("Stopping the VM {instance}…"),
        }
    }
}

/// What the window shows of the VM while it is not connected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub instance: String,
    pub status: Status,
    pub state: State,
    /// What the last dial was doing or why it stopped.
    pub note: Option<String>,
    /// A start is under way.
    pub starting: bool,
}

#[derive(Clone)]
pub struct Gate {
    info: Option<BundleInfo>,
    install_dir: PathBuf,
    keychain: Keychain,
    shared: Arc<Mutex<Shared>>,
    log: PathBuf,
}

#[derive(Default)]
struct Shared {
    status: Option<Status>,
    note: Option<String>,
    starting: bool,
    forward: Option<Forward>,
    checking: bool,
    checked: Option<Instant>,
    busy: Option<&'static str>,
    confirm_stop: bool,
    output: Option<String>,
    output_checked: Option<Instant>,
    reading_output: bool,
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
        let instance = info.as_ref().map_or("atomics", |info| info.instance.as_str());
        let log = atomics_core::paths::atomics_home().join("vm").join(format!("{instance}-controls.log"));
        Self { info, install_dir, keychain: Keychain::current(), shared: Arc::default(), log }
    }

    /// A gate for the bundle `info` in `install_dir`, its token in `keychain`.
    #[cfg(test)]
    pub(crate) fn with(info: Option<BundleInfo>, install_dir: PathBuf, keychain: Keychain) -> Self {
        let log = install_dir.join("controls.log");
        Self { info, install_dir, keychain, shared: Arc::default(), log }
    }

    #[cfg(test)]
    pub(crate) fn set_test_status(&self, status: Status) {
        let mut shared = self.shared.lock().unwrap();
        shared.status = Some(status);
        shared.checked = Some(Instant::now());
    }

    #[cfg(test)]
    pub(crate) fn set_test_busy(&self, busy: Option<&'static str>) {
        self.shared.lock().unwrap().busy = busy;
    }

    pub fn instance(&self) -> Option<&str> {
        self.info.as_ref().map(|info| info.instance.as_str())
    }

    pub fn view_for_host(&self, link: &LinkStatus) -> View {
        let shared = self.shared.lock().unwrap();
        let status = shared.status.clone().unwrap_or(Status::Unknown("checking".into()));
        View {
            instance: self.instance().unwrap_or("atomics").to_string(),
            state: host_status(&status, link, shared.busy),
            status,
            note: shared.note.clone(),
            starting: shared.starting,
        }
    }

    /// A visible window refreshes Lima's state, including while the driver is stopped by an auth error.
    /// No timer or thread keeps polling after frames stop being drawn.
    pub fn poll(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        let Some(info) = self.info.clone() else { return };
        {
            let mut shared = self.shared.lock().unwrap();
            if shared.checking
                || shared.starting
                || shared.checked.is_some_and(|at| at.elapsed() < Duration::from_secs(5))
            {
                return;
            }
            shared.checking = true;
        }
        let shared = self.shared.clone();
        if std::thread::Builder::new()
            .name("atomics-vm-status".into())
            .spawn(move || {
                let status = crate::vm::status(&info.instance, false);
                let mut shared = shared.lock().unwrap();
                if !shared.starting {
                    shared.status = Some(status);
                }
                shared.checking = false;
                shared.checked = Some(Instant::now());
                drop(shared);
                wake();
            })
            .is_err()
        {
            self.shared.lock().unwrap().checking = false;
        }
    }

    pub fn controls(&self, ui: &mut egui::Ui, link: &LinkStatus, wake: Arc<dyn Fn() + Send + Sync>) {
        self.request_output(wake.clone());
        let view = self.view_for_host(link);
        let (busy, confirm_stop) = {
            let shared = self.shared.lock().unwrap();
            (shared.busy, shared.confirm_stop)
        };
        ui.strong("VM");
        ui.label(view.state.headline(&view.instance));
        if let Some(action) = busy.filter(|action| *action != "stop") {
            ui.label(format!("VM {action} in progress…"));
        }
        let idle = busy.is_none() && !view.starting;
        ui.horizontal(|ui| {
            if matches!(view.status, Status::Stopped | Status::Unknown(_))
                && ui.add_enabled(idle, egui::Button::new("Start")).clicked()
            {
                self.start(wake.clone());
            }
            if ui
                .add_enabled(
                    idle && matches!(view.status, Status::Running | Status::Starting),
                    egui::Button::new("Stop"),
                )
                .clicked()
            {
                self.shared.lock().unwrap().confirm_stop = true;
            }
            if ui.add_enabled(idle && view.status == Status::Running, egui::Button::new("Check")).clicked() {
                self.command("check", wake.clone());
            }
            if ui.add_enabled(idle && !matches!(view.status, Status::NotSetUp), egui::Button::new("Update")).clicked() {
                self.command("update", wake.clone());
            }
        });
        if confirm_stop {
            ui.label("Stop the VM? Agents running in it stop with it.");
            ui.horizontal(|ui| {
                if ui.button("Stop VM").clicked() {
                    self.command("stop", wake.clone());
                }
                if ui.button("Cancel").clicked() {
                    self.shared.lock().unwrap().confirm_stop = false;
                }
            });
        }
        if let Some(note) = view.note {
            ui.label(note);
        }
        let output = self.shared.lock().unwrap().output.clone();
        if let Some(output) = output {
            egui::CollapsingHeader::new("VM command output").default_open(true).show(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("vm-command-output").max_height(240.0).stick_to_bottom(true).show(
                    ui,
                    |ui| {
                        ui.add(egui::Label::new(egui::RichText::new(output).monospace()).selectable(true));
                    },
                );
            });
        }
        if busy.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
    }

    fn log_path(&self) -> &Path {
        &self.log
    }

    /// The Machine window draws cached text. Only this worker opens or reads the output file.
    fn request_output(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        {
            let mut shared = self.shared.lock().unwrap();
            let interval = if shared.busy.is_some() { Duration::from_millis(250) } else { Duration::from_secs(5) };
            if shared.reading_output || shared.output_checked.is_some_and(|at| at.elapsed() < interval) {
                return;
            }
            shared.reading_output = true;
        }
        let gate = self.clone();
        if std::thread::Builder::new()
            .name("atomics-vm-output".into())
            .spawn(move || {
                let output = read_output(gate.log_path()).ok();
                let mut shared = gate.shared.lock().unwrap();
                let changed = shared.output != output;
                shared.output = output;
                shared.output_checked = Some(Instant::now());
                shared.reading_output = false;
                drop(shared);
                if changed {
                    wake();
                }
            })
            .is_err()
        {
            self.shared.lock().unwrap().reading_output = false;
        }
    }

    fn command(&self, action: &'static str, wake: Arc<dyn Fn() + Send + Sync>) {
        {
            let mut shared = self.shared.lock().unwrap();
            if shared.busy.is_some() || shared.starting {
                return;
            }
            shared.busy = Some(action);
            shared.confirm_stop = false;
            shared.note = None;
            shared.output_checked = None;
        }
        let gate = self.clone();
        let spawned = std::thread::Builder::new().name("atomics-vm-command".into()).spawn(move || {
            let result = run_command(crate::setup::executable_path(), action, gate.log_path());
            let mut shared = gate.shared.lock().unwrap();
            shared.busy = None;
            shared.checked = None;
            shared.output_checked = None;
            shared.note = Some(match result {
                Ok(()) => format!("VM {action} completed"),
                Err(error) => format!("VM {action} failed: {error:#}"),
            });
            drop(shared);
            wake();
        });
        if let Err(error) = spawned {
            let mut shared = self.shared.lock().unwrap();
            shared.busy = None;
            shared.note = Some(format!("Could not start VM {action}: {error}"));
        }
    }

    /// Start the VM on a thread of its own (through the QA queue); `done` is called when it is up or failed.
    pub fn start(&self, done: Arc<dyn Fn() + Send + Sync>) {
        let Some(info) = self.info.clone() else { return };
        {
            let mut shared = self.shared.lock().unwrap();
            if shared.starting || shared.busy.is_some() {
                return;
            }
            shared.starting = true;
            shared.status = Some(Status::Starting);
            shared.note = Some(format!("Starting the VM {}…", info.instance));
        }
        let shared = self.shared.clone();
        let _ = std::thread::Builder::new().name("atomics-vm-start".into()).spawn(move || {
            let result = crate::vm::start(&info.instance);
            let status = crate::vm::status(&info.instance, false);
            {
                let mut shared = shared.lock().unwrap();
                shared.starting = false;
                shared.note = result.err().map(|e| format!("{e:#}"));
                shared.status = Some(status);
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

fn host_status(status: &Status, link: &LinkStatus, busy: Option<&str>) -> State {
    if busy == Some("stop") {
        State::Stopping
    } else if *status == Status::Running && *link == LinkStatus::Stopped(Stop::Unauthorised) {
        State::Unauthorised
    } else if *status == Status::Running && *link != LinkStatus::Live {
        State::HostDisconnected
    } else {
        State::Vm(status.clone())
    }
}

fn read_output(path: &Path) -> Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(128 * 1024)))?;
    let mut bytes = Vec::new();
    file.take(128 * 1024).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Updates close this window. A separate session survives that close, with output retained for the
/// reopened Machine window. Neither stdout nor stderr can fill a pipe while the UI is gone.
fn run_command(executable: std::io::Result<PathBuf>, action: &str, log: &Path) -> Result<()> {
    use std::io::Write;
    use std::os::unix::process::CommandExt;
    std::fs::create_dir_all(log.parent().context("the VM log's folder")?)?;
    let mut output = std::fs::File::create(log)?;
    writeln!(output, "$ atomics vm {action}")?;
    let mut command = Command::new(executable?);
    command.args(["vm", action]).stdin(Stdio::null()).stdout(output.try_clone()?).stderr(output.try_clone()?);
    // SAFETY: only async-signal-safe calls run before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::signal(libc::SIGHUP, libc::SIG_IGN) == libc::SIG_ERR {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let status = command.status().context("running the VM command")?;
    writeln!(output, "\nVM {action}: {status}")?;
    if !status.success() {
        bail!("{status}; see the command output");
    }
    Ok(())
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

    #[test]
    fn instance_and_host_states_are_distinct_and_stopping_takes_precedence() {
        assert_eq!(host_status(&Status::Running, &LinkStatus::Connecting, None), State::HostDisconnected);
        assert_eq!(host_status(&Status::Running, &LinkStatus::Live, None), State::Vm(Status::Running));
        assert_eq!(host_status(&Status::Starting, &LinkStatus::Connecting, None), State::Vm(Status::Starting));
        assert_eq!(host_status(&Status::Stopped, &LinkStatus::Connecting, None), State::Vm(Status::Stopped));
        assert_eq!(host_status(&Status::NotSetUp, &LinkStatus::Connecting, None), State::Vm(Status::NotSetUp));
        let refused = LinkStatus::Stopped(Stop::Unauthorised);
        assert_eq!(host_status(&Status::Running, &refused, None), State::Unauthorised);
        assert_eq!(host_status(&Status::Running, &refused, Some("stop")), State::Stopping);
    }

    #[test]
    fn vm_commands_keep_both_output_streams_and_the_exit_status() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("atomics");
        crate::test_support::write_program(
            &exe,
            "#!/bin/sh\nkill -HUP \"$$\"\necho \"output: $*\"\necho error-output >&2\nexit 7\n",
        );
        let log = dir.path().join("vm/controls.log");
        assert!(run_command(Ok(exe), "check", &log).is_err());
        let output = read_output(&log).unwrap();
        assert!(
            output.contains("output: vm check") && output.contains("error-output") && output.contains("7"),
            "{output}"
        );
    }

    #[test]
    fn machine_output_is_cached_between_background_reads() {
        let dir = tempfile::tempdir().unwrap();
        let gate = Gate::with(None, dir.path().into(), Keychain::in_folder(dir.path()));
        std::fs::write(gate.log_path(), "first output").unwrap();
        let wait = |expected: &str| {
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(2));
            loop {
                let shared = gate.shared.lock().unwrap();
                if !shared.reading_output && shared.output.as_deref() == Some(expected) {
                    break;
                }
                drop(shared);
                assert!(Instant::now() < deadline, "the output worker did not publish {expected}");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        gate.request_output(Arc::new(|| {}));
        wait("first output");
        let checked = gate.shared.lock().unwrap().output_checked;
        std::fs::write(gate.log_path(), "second output").unwrap();
        for _ in 0..20 {
            gate.request_output(Arc::new(|| {}));
        }
        assert_eq!(gate.shared.lock().unwrap().output_checked, checked);
        assert_eq!(gate.shared.lock().unwrap().output.as_deref(), Some("first output"));
        gate.shared.lock().unwrap().output_checked = None;
        gate.request_output(Arc::new(|| {}));
        wait("second output");
    }

    #[test]
    fn stop_requires_confirmation_and_the_machine_controls_are_visible() {
        let dir = tempfile::tempdir().unwrap();
        let info = bundle(dir.path(), 47800, "#!/bin/sh\nexit 1\n");
        let gate = Gate::with(Some(info), dir.path().into(), Keychain::in_folder(dir.path()));
        gate.set_test_status(Status::Running);
        let ctx = egui::Context::default();
        let frame = |events| {
            let mut output = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| {
                gate.controls(ui, &LinkStatus::Live, Arc::new(|| {}))
            });
            output.textures_delta.clear();
            output
                .shapes
                .into_iter()
                .filter_map(|shape| match shape.shape {
                    egui::Shape::Text(text) => Some((text.galley.text().to_string(), text.visual_bounding_rect())),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let shown = frame(Vec::new());
        for label in ["VM", "Stop", "Check", "Update"] {
            assert!(shown.iter().any(|(text, _)| text == label), "missing {label}: {shown:?}");
        }
        let pos = shown.iter().find(|(text, _)| text == "Stop").unwrap().1.center();
        frame(vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            },
        ]);
        frame(vec![egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: Default::default(),
        }]);
        let confirmed = frame(Vec::new());
        assert!(
            confirmed.iter().any(|(text, _)| text == "Stop the VM? Agents running in it stop with it."),
            "{confirmed:?}"
        );
        assert!(gate.shared.lock().unwrap().busy.is_none(), "Stop must ask before running a command");
    }

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
