//! The ATOMICS VM on a Mac (docs/nixos-vm.md): its Lima instance, the Mac bundle built with it, and
//! `atomics vm setup|start|stop|status|check|config`. Everything runs Lima's own `limactl` and the
//! bundle's scripts; nothing here signs in anywhere, and nothing replaces an instance or writes a
//! config file without the user saying so.
//!
//! The bundle (`scripts/macos-vm-build.sh`) names its Lima instance and the VM's user in
//! `atomics-vm.json`, so every command here works on the instance the bundle was built for. An
//! instance `atomics vm setup` did not create (an older one of the same name) is never reused or
//! replaced unless the user confirms it.

pub mod download;
pub mod secrets;
pub mod setup;
pub mod update;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// What `atomics-vm.json` in the bundle says about the VM it was built with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleInfo {
    pub version: u32,
    /// The Lima instance the bundle's wrappers and forward reach.
    pub instance: String,
    /// The VM's login user.
    pub user: String,
    /// Its home folder in the VM.
    pub home: String,
    /// The Mac account the VM mounts folders of.
    pub mac_user: String,
    /// Where the bundle is installed on the Mac.
    pub install_dir: String,
    /// The ATOMICS host in the VM, when the bundle was built with it (`--host`).
    #[serde(default)]
    pub host: Option<HostInfo>,
    /// The guest user that builds and switches the system when the VM's own user may not use sudo
    /// (`atomics.vm.hardened`). Updates reach it over Lima's ssh config, with the key only this Mac has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin: Option<String>,
    /// Build inputs saved by newer bundles; older bundles recover these from Lima's config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildInfo {
    pub cpus: u32,
    #[serde(rename = "memoryGiB")]
    pub memory_gib: u32,
    #[serde(rename = "diskGiB")]
    pub disk_gib: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    /// The protocol server's port on the guest's loopback; the bundle's forward opens the same port
    /// on the Mac's loopback.
    pub port: u16,
}

pub const BUNDLE_INFO: &str = "atomics-vm.json";

impl BundleInfo {
    pub fn read(bundle: &Path) -> Result<Self> {
        let file = bundle.join(BUNDLE_INFO);
        let text = std::fs::read_to_string(&file).with_context(|| {
            format!("reading {} (rebuild the bundle with scripts/macos-vm-build.sh)", file.display())
        })?;
        let info: Self = serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
        if info.version != 1 {
            bail!("{} is version {}; this ATOMICS reads version 1", file.display(), info.version);
        }
        if !valid_instance(&info.instance) {
            bail!("{} names an invalid Lima instance", file.display());
        }
        Ok(info)
    }

    /// The bundle installed on this Mac, if any: `ATOMICS_VM_BUNDLE`, else the default folder.
    pub fn installed() -> Option<(PathBuf, Self)> {
        let dir = installed_dir();
        Self::read(&dir).ok().map(|info| (dir, info))
    }
}

/// Where the Mac bundle is installed (the module's default `installDir`).
pub fn installed_dir() -> PathBuf {
    atomics_core::paths::env_override("VM_BUNDLE")
        .map(PathBuf::from)
        .unwrap_or_else(|| atomics_core::paths::home().join(".local/share/atomics-vm"))
}

pub fn valid_instance(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// The VM as the UI and `atomics vm status` show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// No bundle is installed, or its instance does not exist yet.
    NotSetUp,
    Stopped,
    /// `limactl start` is running (here or elsewhere), or the instance is up but its host is not yet.
    Starting,
    Running,
    /// Lima reports something else (`Broken`), or `limactl` could not be asked.
    Unknown(String),
}

impl Status {
    pub fn indicator(&self, busy: Option<&str>) -> &'static str {
        match (self, busy) {
            (_, Some("stopping")) => "VM stopping",
            (Status::Starting, _) | (_, Some(_)) => "VM starting",
            (Status::Running, _) => "VM running",
            (Status::Stopped, _) => "VM stopped",
            (Status::NotSetUp, _) => "VM not set up",
            (Status::Unknown(_), _) => "VM unknown",
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Status::NotSetUp => "not set up",
            Status::Stopped => "stopped",
            Status::Starting => "starting",
            Status::Running => "running",
            Status::Unknown(what) => what,
        }
    }
}

/// `limactl`, found on the login shell's PATH or where Homebrew puts it; a Mac app's own PATH has neither.
pub fn limactl() -> Option<PathBuf> {
    crate::setup::find_program("limactl").or_else(|| {
        ["/opt/homebrew/bin/limactl", "/usr/local/bin/limactl"].iter().map(PathBuf::from).find(|path| path.is_file())
    })
}

fn limactl_command() -> Result<Command> {
    let program = limactl().context("limactl is not installed; install Lima 2.2 or newer (brew install lima)")?;
    let mut command = Command::new(program);
    // limactl asks before some actions on a terminal; here it never may.
    command.env("LIMA_TTY", "0").stdin(Stdio::null());
    Ok(command)
}

/// One instance as `limactl list --json` lists it.
#[derive(Debug, Clone, Deserialize)]
struct Listed {
    name: String,
    status: String,
}

/// Lima's status for `instance`, or `None` when it does not exist.
pub fn lima_status(instance: &str) -> Result<Option<String>> {
    Ok(listed_status(instance, &lima_list()?, None, &|warning| eprintln!("{warning}")))
}

/// Explicit commands may remove obsolete setup records. Background status polls are read-only.
pub fn lima_status_for_command(instance: &str) -> Result<Option<String>> {
    Ok(listed_status(instance, &lima_list()?, Some(&record_dir()), &|warning| eprintln!("{warning}")))
}

fn lima_list() -> Result<Vec<Listed>> {
    let output = limactl_command()?.args(["list", "--json"]).output().context("running limactl list")?;
    if !output.status.success() {
        bail!("limactl list failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    parse_list(&String::from_utf8_lossy(&output.stdout))
}

fn listed_status(instance: &str, listed: &[Listed], cleanup: Option<&Path>, warn: &dyn Fn(String)) -> Option<String> {
    if let Some(dir) = cleanup
        && let Err(error) = prune_records(dir, listed)
    {
        warn(format!("warning: could not remove obsolete VM setup records: {error:#}"));
    }
    listed.iter().find(|entry| entry.name == instance).map(|entry| entry.status.clone())
}

fn parse_list(text: &str) -> Result<Vec<Listed>> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).context("reading limactl list"))
        .collect()
}

/// Only a successful, fully parsed list proves that an instance was deleted. Never prune on errors.
fn prune_records(dir: &Path, listed: &[Listed]) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type().is_ok_and(|kind| kind.is_file())
            || entry.path().extension().is_none_or(|ext| ext != "json")
        {
            continue;
        }
        let record = std::fs::read(entry.path()).ok().and_then(|data| serde_json::from_slice::<Record>(&data).ok());
        if let Some(record) = record
            && valid_instance(&record.instance)
            && entry.file_name() == format!("{}.json", record.instance).as_str()
            && !listed.iter().any(|instance| instance.name == record.instance)
        {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

/// The VM's status for `instance`; `starting` while a start this process made is under way.
pub fn status(instance: &str, starting: bool) -> Status {
    status_from_lima(lima_status(instance), starting)
}

pub fn status_for_command(instance: &str) -> Status {
    status_from_lima(lima_status_for_command(instance), false)
}

fn status_from_lima(result: Result<Option<String>>, starting: bool) -> Status {
    match result {
        Ok(None) => Status::NotSetUp,
        Ok(Some(_)) if starting => Status::Starting,
        Ok(Some(state)) => match state.as_str() {
            "Running" => Status::Running,
            "Stopped" => Status::Stopped,
            other => Status::Unknown(other.to_lowercase()),
        },
        Err(error) => Status::Unknown(format!("{error:#}")),
    }
}

/// The record `atomics vm setup` keeps of each instance it created, so it reuses those and never
/// takes over one it did not make.
fn record_dir() -> PathBuf {
    atomics_core::paths::atomics_home().join("vm")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    instance: String,
    image: String,
    created_at: u64,
}

pub fn created_here(instance: &str) -> bool {
    std::fs::read_to_string(record_dir().join(format!("{instance}.json")))
        .ok()
        .and_then(|text| serde_json::from_str::<Record>(&text).ok())
        .is_some_and(|record| record.instance == instance)
}

fn remember(instance: &str, image: &Path) -> Result<()> {
    let dir = record_dir();
    std::fs::create_dir_all(&dir)?;
    let record = Record {
        instance: instance.to_string(),
        image: image.display().to_string(),
        created_at: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
    };
    std::fs::write(dir.join(format!("{instance}.json")), serde_json::to_string_pretty(&record)?)?;
    Ok(())
}

fn forget(instance: &str) {
    let _ = std::fs::remove_file(record_dir().join(format!("{instance}.json")));
}

/// Runs `command` through `atomics queue --class qa`, so the VM's start and its checks wait for a QA
/// slot on a shared machine. Inherits stdio; returns whether it succeeded.
pub fn queued(command: &[String]) -> bool {
    let socket = atomics_core::paths::socket_path();
    queued_at(&socket, BundleInfo::installed().is_some_and(|(_, info)| info.host.is_some()), command)
}

fn queued_at(socket: &Path, vm_client: bool, command: &[String]) -> bool {
    use std::os::unix::net::UnixStream;
    let absent = vm_client
        && matches!(
            UnixStream::connect(socket).err().map(|error| error.kind()),
            Some(std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused)
        );
    if absent {
        // A VM client has no Mac host or local queue; its VM must start before its host can queue.
        return command.split_first().is_some_and(|(program, args)| {
            Command::new(program).args(args).status().is_ok_and(|status| status.success())
        });
    }
    crate::governor::run_queued(socket, atomics_core::governor::SlotClass::Qa, command) == 0
}

/// Starts the instance (through the QA queue) and waits until Lima reports it running.
pub fn start(instance: &str) -> Result<()> {
    let program = limactl().context("limactl is not installed")?;
    let command = [program.display().to_string(), "start".into(), "--tty=false".into(), instance.into()];
    if !queued(&command) {
        bail!("limactl start {instance} failed; `limactl list` and ~/.lima/{instance}/ha.stderr.log say why");
    }
    Ok(())
}

pub fn stop(instance: &str) -> Result<()> {
    let status = limactl_command()?
        .args(["stop", instance])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("running limactl stop")?;
    if !status.success() {
        bail!("limactl stop {instance} failed");
    }
    Ok(())
}

/// `limactl shell` in the guest's home, as the VM's user, with a login shell's PATH.
pub fn guest_shell(info: &BundleInfo) -> Result<Command> {
    let mut command = limactl_command()?;
    command.args(["shell", &format!("--workdir={}", info.home), &info.instance]);
    Ok(command)
}

/// A bash in the guest that runs the script on its stdin, for changes to the system: as the admin user over
/// Lima's ssh config in hardened mode, where the VM's own user has no sudo, and as that user otherwise. A login
/// shell sets PATH and the profile up, then hands over to a plain bash: an `exit` in a login shell would source
/// /etc/bash_logout, which fails under a script's `set -u`.
pub fn system_shell(info: &BundleInfo) -> Result<Command> {
    system_command(info, "exec bash -s")
}

/// Fixed administrator command, leaving stdin entirely available for private data. Shares the
/// update path's account and explicitly disables SSH connection sharing in hardened mode.
pub(super) fn system_command(info: &BundleInfo, script: &str) -> Result<Command> {
    system_command_with(info, script, || guest_shell(info))
}

fn system_command_with(info: &BundleInfo, script: &str, guest: impl FnOnce() -> Result<Command>) -> Result<Command> {
    let Some(admin) = &info.admin else {
        let mut command = guest()?;
        command.args(["bash", "--login", "-c", script]);
        return Ok(command);
    };
    let mut command = Command::new("ssh");
    command
        .arg("-F")
        .arg(lima_home().join(&info.instance).join("ssh.config"))
        .args(["-o", "LogLevel=ERROR", "-o", "ControlMaster=no", "-o", "ControlPath=none", "-T", "-l", admin])
        .arg(format!("lima-{}", info.instance))
        // ssh hands the remote shell one command line.
        .arg(format!("bash --login -c '{}'", script.replace('\'', "'\\''")));
    Ok(command)
}

/// Where Lima keeps its instances.
fn lima_home() -> PathBuf {
    std::env::var_os("LIMA_HOME").map(PathBuf::from).unwrap_or_else(|| atomics_core::paths::home().join(".lima"))
}

/// The VM's status, kept current on a thread of its own for the UI: Lima is asked every few seconds
/// while someone looks, and a start or stop runs there too, so the UI never waits on `limactl`.
#[derive(Clone, Default)]
pub struct Monitor {
    shared: Arc<Mutex<Shared>>,
    window: crate::host::Window,
}

#[derive(Default)]
struct Shared {
    /// The installed bundle's instance, when one is installed.
    instance: Option<String>,
    status: Option<Status>,
    /// A start or stop under way, and what it is.
    busy: Option<&'static str>,
    /// Why the last start or stop failed.
    error: Option<String>,
    viewed: Option<Instant>,
    checked: Option<Instant>,
    refresh_requested: bool,
    worker: Option<std::thread::Thread>,
}

/// How often the UI's view of the VM is refreshed.
const POLL: Duration = Duration::from_secs(5);

/// What the UI shows of the VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub instance: String,
    pub status: Status,
    pub busy: Option<&'static str>,
    pub error: Option<String>,
}

impl Monitor {
    #[cfg(test)]
    pub(crate) fn with_view(view: View) -> Self {
        let monitor = Self::default();
        *monitor.shared.lock().unwrap() = Shared {
            instance: Some(view.instance),
            status: Some(view.status),
            busy: view.busy,
            error: view.error,
            ..Default::default()
        };
        monitor
    }
    /// Only on macOS, and only with a bundle installed: elsewhere the UI shows no VM at all. `window` is
    /// asked to repaint when what it shows changes.
    pub fn start(window: crate::host::Window) -> Option<Self> {
        if !cfg!(target_os = "macos") || cfg!(test) {
            return None;
        }
        let (_, info) = BundleInfo::installed()?;
        let monitor = Monitor { window, ..Monitor::default() };
        monitor.shared.lock().unwrap().instance = Some(info.instance);
        let shared = Arc::downgrade(&monitor.shared);
        let window = monitor.window.clone();
        std::thread::Builder::new()
            .name("atomics-vm-monitor".into())
            .spawn(move || {
                if let Some(shared) = shared.upgrade() {
                    shared.lock().unwrap().worker = Some(std::thread::current());
                }
                while let Some(shared) = shared.upgrade() {
                    let refresh_due = {
                        let shared = shared.lock().unwrap();
                        recently_viewed(shared.viewed, Instant::now())
                            && (shared.refresh_requested
                                || shared.checked.is_none_or(|checked| checked.elapsed() >= POLL))
                    };
                    if refresh_due {
                        Monitor { shared: shared.clone(), window: window.clone() }.refresh();
                    }
                    drop(shared);
                    std::thread::park_timeout(POLL);
                }
            })
            .ok()?;
        Some(monitor)
    }

    fn refresh(&self) {
        let (instance, busy) = {
            let shared = self.shared.lock().unwrap();
            (shared.instance.clone(), shared.busy)
        };
        let Some(instance) = instance else { return };
        let status = status(&instance, busy == Some("starting"));
        let mut shared = self.shared.lock().unwrap();
        let changed = shared.status.as_ref() != Some(&status);
        shared.status = Some(status);
        shared.checked = Some(Instant::now());
        shared.refresh_requested = false;
        drop(shared);
        if changed {
            self.window.request_repaint();
        }
    }

    pub fn view(&self) -> Option<View> {
        let mut shared = self.shared.lock().unwrap();
        if shared.viewed.is_none_or(|viewed| viewed.elapsed() >= POLL) {
            shared.refresh_requested = true;
            if let Some(worker) = &shared.worker {
                worker.unpark();
            }
        }
        shared.viewed = Some(Instant::now());
        Some(View {
            instance: shared.instance.clone()?,
            status: shared.status.clone().unwrap_or(Status::Unknown("checking".into())),
            busy: shared.busy,
            error: shared.error.clone(),
        })
    }

    /// Start (`true`) or stop the VM on a thread of its own; the view says so meanwhile.
    pub fn set_running(&self, run: bool) {
        let instance = {
            let mut shared = self.shared.lock().unwrap();
            if shared.busy.is_some() {
                return;
            }
            shared.busy = Some(if run { "starting" } else { "stopping" });
            shared.error = None;
            if run {
                shared.status = Some(Status::Starting);
            }
            shared.instance.clone()
        };
        self.window.request_repaint();
        let Some(instance) = instance else { return };
        let monitor = self.clone();
        let _ = std::thread::Builder::new().name("atomics-vm-start".into()).spawn(move || {
            let result = if run { start(&instance) } else { stop(&instance) };
            {
                let mut shared = monitor.shared.lock().unwrap();
                shared.busy = None;
                shared.error = result.err().map(|e| format!("{e:#}"));
            }
            monitor.refresh();
        });
    }
}

fn recently_viewed(viewed: Option<Instant>, now: Instant) -> bool {
    viewed.is_some_and(|viewed| now.saturating_duration_since(viewed) <= POLL * 2)
}

/// `atomics vm …` on Linux: the VM path is a Mac's.
pub const MACOS_ONLY: &str = "The ATOMICS VM is for macOS: on Linux, ATOMICS and its agents run on the machine itself \
     (docs/nixos-vm.md). Build the VM's image here with `nix build .#atomics-vm-image` for a Mac to use.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lima_lists_one_instance_per_line() {
        let text = concat!(
            r#"{"name":"atomics","status":"Stopped","dir":"/home/user/.lima/atomics","cpus":4}"#,
            "\n",
            r#"{"name":"atomics-host","status":"Running","dir":"/home/user/.lima/atomics-host"}"#,
            "\n"
        );
        let listed = parse_list(text).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!((listed[1].name.as_str(), listed[1].status.as_str()), ("atomics-host", "Running"));
        assert!(parse_list("not json").is_err());
    }

    #[test]
    fn deleted_instances_lose_only_their_own_records() {
        let dir = tempfile::tempdir().unwrap();
        for instance in ["present", "deleted"] {
            let record = Record { instance: instance.into(), image: "image".into(), created_at: 1 };
            std::fs::write(dir.path().join(format!("{instance}.json")), serde_json::to_vec(&record).unwrap()).unwrap();
        }
        std::fs::write(dir.path().join("other.json"), "unrelated data").unwrap();
        std::fs::write(dir.path().join("update.log"), "saved output").unwrap();
        prune_records(dir.path(), &parse_list(r#"{"name":"present","status":"Stopped"}"#).unwrap()).unwrap();
        assert!(dir.path().join("present.json").exists());
        assert!(!dir.path().join("deleted.json").exists());
        assert!(dir.path().join("other.json").exists());
        assert!(dir.path().join("update.log").exists());
    }

    #[test]
    fn monitoring_sleeps_when_the_view_is_no_longer_drawn() {
        let now = Instant::now();
        assert!(!recently_viewed(None, now));
        assert!(recently_viewed(Some(now), now + POLL));
        assert!(!recently_viewed(Some(now), now + POLL * 3));
    }

    #[test]
    fn restoring_a_paused_view_wakes_its_monitor_without_waiting_for_the_poll() {
        let monitor =
            Monitor::with_view(View { instance: "vm-test".into(), status: Status::Stopped, busy: None, error: None });
        monitor.shared.lock().unwrap().viewed = Some(Instant::now() - POLL - Duration::from_millis(1));
        // Resume must request a fresh answer even if the last background check was recent.
        monitor.shared.lock().unwrap().checked = Some(Instant::now());
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (woke_tx, woke_rx) = std::sync::mpsc::channel();
        let shared = monitor.shared.clone();
        let guard = crate::test_support::scaled_timeout(Duration::from_secs(1));
        let worker = std::thread::spawn(move || {
            shared.lock().unwrap().worker = Some(std::thread::current());
            ready_tx.send(()).unwrap();
            std::thread::park_timeout(guard * 5);
            woke_tx.send(()).unwrap();
        });
        ready_rx.recv().unwrap();
        monitor.view();
        woke_rx.recv_timeout(guard).expect("view() must wake a paused status poll immediately");
        assert!(monitor.shared.lock().unwrap().refresh_requested);
        worker.join().unwrap();
    }

    #[test]
    fn status_polls_leave_records_alone_and_command_cleanup_failures_only_warn() {
        let dir = tempfile::tempdir().unwrap();
        let record = Record { instance: "gone".into(), image: "image".into(), created_at: 1 };
        let path = dir.path().join("gone.json");
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        let listed = parse_list(r#"{"name":"vm-test","status":"Running"}"#).unwrap();
        let warnings = std::cell::RefCell::new(Vec::new());
        let warn = |warning| warnings.borrow_mut().push(warning);
        assert_eq!(listed_status("vm-test", &listed, None, &warn).as_deref(), Some("Running"));
        assert!(path.exists(), "a background poll cannot delete a setup record from another Lima home");
        assert_eq!(listed_status("vm-test", &listed, Some(dir.path()), &warn).as_deref(), Some("Running"));
        assert!(!path.exists(), "an explicit command may prune obsolete records");
        let not_a_directory = dir.path().join("file");
        std::fs::write(&not_a_directory, "fixture").unwrap();
        assert_eq!(listed_status("vm-test", &listed, Some(&not_a_directory), &warn).as_deref(), Some("Running"));
        assert_eq!(warnings.borrow().len(), 1);
    }

    #[test]
    fn vm_start_bypasses_a_stale_local_queue_socket() {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("stale.sock");
        drop(UnixListener::bind(&socket).unwrap());
        assert!(socket.exists());
        assert!(queued_at(&socket, true, &["sh".into(), "-c".into(), "exit 0".into()]));
        assert!(!queued_at(&socket, true, &["sh".into(), "-c".into(), "exit 7".into()]));
    }

    #[test]
    fn each_vm_state_has_its_own_indicator() {
        let states =
            [Status::Running, Status::Stopped, Status::Starting, Status::NotSetUp, Status::Unknown("broken".into())];
        let labels: std::collections::HashSet<_> = states.iter().map(|state| state.indicator(None)).collect();
        assert_eq!(labels.len(), states.len());
        assert_eq!(Status::Running.indicator(Some("stopping")), "VM stopping");
    }

    #[test]
    fn the_bundle_names_its_instance_and_user() {
        let dir = tempfile::tempdir().unwrap();
        let text = r#"{"version":1,"instance":"atomics-host","user":"atomics","home":"/home/atomics",
            "macUser":"user","installDir":"/Users/user/.local/share/atomics-vm","host":{"port":47800}}"#;
        std::fs::write(dir.path().join(BUNDLE_INFO), text).unwrap();
        let info = BundleInfo::read(dir.path()).unwrap();
        assert_eq!(info.instance, "atomics-host");
        assert_eq!(info.host, Some(HostInfo { port: 47800 }));
        std::fs::write(dir.path().join(BUNDLE_INFO), text.replace("atomics-host", "a b")).unwrap();
        assert!(BundleInfo::read(dir.path()).is_err(), "an instance name a shell would split");
        std::fs::write(dir.path().join(BUNDLE_INFO), text.replace("\"version\":1", "\"version\":2")).unwrap();
        assert!(BundleInfo::read(dir.path()).is_err(), "a newer bundle format");
    }

    #[test]
    fn instance_names_are_limas() {
        assert!(valid_instance("atomics-host"));
        assert!(valid_instance("a.b_c-1"));
        for bad in ["", "a b", "a/b", "a;b", "~x"] {
            assert!(!valid_instance(bad), "{bad}");
        }
    }
}
