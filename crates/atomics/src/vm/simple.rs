//! The optional Mac onboarding path. Progress is saved independently of the Full setup and config.
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use atomics_core::projection::{SetupAction, SetupClick, SetupStep, SimpleSetupInfo};
use serde::{Deserialize, Serialize};

use SetupAction::*;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub selected: bool,
    pub pending: bool,
    pub done: Vec<SetupAction>,
    pub download: Option<PathBuf>,
    #[cfg(test)]
    #[serde(skip)]
    pub program: Option<PathBuf>,
}

impl State {
    pub fn load() -> Self {
        std::fs::read(path()).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let file = path();
        let parent = file.parent().context("setup state folder")?;
        std::fs::create_dir_all(parent)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(&mut temp, self)?;
        temp.persist(file)?;
        Ok(())
    }

    pub fn choose(&mut self, simple: bool) -> Result<()> {
        self.selected = simple;
        self.pending = true;
        self.save()
    }

    pub fn complete(&mut self, action: SetupAction) -> Result<()> {
        if !self.done.contains(&action) {
            self.done.push(action);
        }
        self.save()
    }

    /// Remove only an image staged by this wizard. A local --from bundle never belongs to it.
    pub fn discard_download(&mut self) -> Result<()> {
        if let Some(folder) = &self.download {
            let owned = folder.parent() == Some(atomics_core::paths::atomics_home().join("setup-downloads").as_path())
                && folder
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| uuid::Uuid::parse_str(name).is_ok());
            if owned && folder.exists() {
                std::fs::remove_dir_all(folder)?;
            }
        }
        self.download = None;
        Ok(())
    }

    pub fn info(&self) -> SimpleSetupInfo {
        let row = |action, name: &str, note: &str, label: &str, command: String| SetupStep {
            name: name.into(),
            done: self.done.contains(&action),
            note: note.into(),
            clicks: vec![SetupClick::new(action, label)],
            command: Some(command),
        };
        let onboard = super::BundleInfo::installed().map_or_else(
            || "limactl shell --workdir=/home/atomics atomics atomics-onboard".into(),
            |(_, info)| format!("limactl shell --workdir={} {} atomics-onboard", info.home, info.instance),
        );
        let mut sign_in = row(
            CheckVmLogins,
            "Sign in inside the VM",
            "Open the guest terminal here. The agents' own prompts guide sign-in; credentials stay in the VM.",
            "Check sign-ins",
            format!("{onboard} --check"),
        );
        sign_in.clicks.insert(0, SetupClick::new(VmLogin, "Open sign-in terminal"));
        SimpleSetupInfo {
            selected: self.selected,
            steps: vec![
                row(
                    CheckVmRequirements,
                    "Lima and Rosetta",
                    "Needs an Apple Silicon Mac, Lima 2.2 or newer, and Rosetta. Install Lima with Homebrew; Rosetta uses Apple's installer.",
                    "Check this Mac",
                    "brew install lima\nsoftwareupdate --install-rosetta --agree-to-license".into(),
                ),
                row(
                    DownloadVm,
                    "Download the signed VM",
                    "Downloads this release's hardened image and verifies its checksum and signature. A missing or invalid image stops here.",
                    "Download VM",
                    "atomics vm download --hardened --to /tmp/atomics-vm-download".into(),
                ),
                row(
                    CreateVm,
                    "Set up the hardened VM",
                    "Creates and checks the VM. Agents have no sudo, Docker is rootless, and Mac project folders are read-only. Existing VMs are kept.",
                    "Set up VM",
                    "atomics vm setup --simple".into(),
                ),
                sign_in,
                row(
                    ConnectVm,
                    "Connect to the VM host",
                    "Checks the host connection and saves it for this window. The window reopens on the VM when you finish below.",
                    "Connect VM",
                    "atomics vm config --apply --check-connection".into(),
                ),
                row(
                    SimpleMode,
                    "Use simple mode",
                    "Saves simple mode and reopens this window on the VM. The optional network policy is off; you can configure it later.",
                    "Finish Simple setup",
                    "atomics vm config --apply --simple".into(),
                ),
            ],
        }
    }

    pub fn ready(&self, action: SetupAction) -> Result<()> {
        let requirement = match action {
            DownloadVm => Some((CheckVmRequirements, "Check Lima and Rosetta first")),
            CreateVm => Some((DownloadVm, "Download and verify the VM first")),
            VmLogin | CheckVmLogins | ConnectVm => Some((CreateVm, "Set up the hardened VM first")),
            SimpleMode => Some((ConnectVm, "Connect to the VM host first")),
            _ => None,
        };
        if let Some((step, why)) = requirement
            && !self.done.contains(&step)
        {
            bail!("{why}; this step can wait until then");
        }
        Ok(())
    }

    /// Commands are arguments, never interpolated shell input. The same commands remain available from a terminal.
    pub fn command(&self, action: SetupAction, program: &Path) -> Result<Command> {
        let mut command = Command::new(program);
        match action {
            CheckVmRequirements => {
                command.args(["vm", "requirements"]);
            }
            DownloadVm => {
                command
                    .args(["vm", "download", "--hardened", "--to"])
                    .arg(self.download.as_ref().context("no download folder")?);
            }
            CreateVm => {
                if let Some(download) = &self.download {
                    command.args(["vm", "setup", "--simple", "--from"]).arg(download);
                } else if self.done.contains(&CreateVm) {
                    owned_bundle()?;
                    command.args(["vm", "check"]);
                } else {
                    bail!("the download is missing; download again");
                }
            }
            CheckVmLogins => {
                let info = owned_bundle()?;
                command = super::guest_shell(&info)?;
                command.args(["atomics-onboard", "--check"]);
            }
            ConnectVm => {
                command.args(["vm", "config", "--apply", "--check-connection"]);
            }
            SimpleMode => {
                command.args(["vm", "config", "--apply", "--simple"]);
            }
            _ => bail!("this setup action is handled by the window"),
        }
        Ok(command)
    }
}

fn path() -> PathBuf {
    atomics_core::paths::atomics_home().join("simple-setup.json")
}

pub fn select() -> Result<()> {
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        bail!("Simple VM setup requires an Apple Silicon Mac. Full setup remains available.");
    }
    State::load().choose(true)
}

pub fn requirements() -> Result<()> {
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        bail!("The release VM requires an Apple Silicon Mac");
    }
    super::setup::check_mac()
}

pub(crate) fn owned_bundle() -> Result<super::BundleInfo> {
    let (_, info) = super::BundleInfo::installed().context("No VM bundle is installed; set up the VM first")?;
    super::require_simple_owner(&info.instance)?;
    Ok(info)
}

/// Uses the config editor on the VM host's own layer. Only the mode fragment is supplied from the Mac;
/// the guest's existing text is edited in memory and returned with an optimistic before-value check.
pub fn configure(simple: bool) -> Result<()> {
    let info = owned_bundle()?;
    if info.flavor != super::Flavor::Hardened {
        bail!("Simple setup requires the hardened VM flavor; the existing VM is kept");
    }
    super::built::require_flavor(&info, &super::built::probe(&info)?)?;
    println!("Pairing with the VM host and checking its socket forward…");
    let gate = crate::client::vm::Gate::new();
    let _endpoint = gate.resolver()()?;
    if simple {
        set_guest_mode(&info)?;
    }
    super::setup::config(true)?;
    println!("VM host connection ready{}", if simple { "; simple mode saved on the VM host" } else { "" });
    Ok(())
}

const READ_CONFIG: &str = r#"import json, os, pathlib, socket
base = pathlib.Path(os.environ.get('ATOMICS_CONFIG') or pathlib.Path(os.environ.get('XDG_CONFIG_HOME', str(pathlib.Path.home()/'.config')))/'atomics/config.toml')
host = socket.gethostname().removesuffix('.local')
if not host or any(c not in 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-' for c in host) or host in ('.', '..'): raise SystemExit('invalid VM hostname')
file = base.parent/'config.d'/(host+'.toml')
print(json.dumps({'file': str(file), 'before': file.read_text() if file.exists() else None}))
"#;

const WRITE_CONFIG: &str = r#"import json, os, pathlib, sys, tempfile, time
edit = json.load(sys.stdin)
file = pathlib.Path(edit['file'])
before = file.read_text() if file.exists() else None
if before != edit['before']: raise SystemExit('VM config changed meanwhile; retry this step')
file.parent.mkdir(parents=True, exist_ok=True)
if before is not None:
    backup = file.with_name(file.name+'.'+str(time.time_ns())+'.bak')
    fd = os.open(backup, os.O_WRONLY|os.O_CREAT|os.O_EXCL, 0o600)
    with os.fdopen(fd, 'w') as out: out.write(before)
fd, staging = tempfile.mkstemp(dir=file.parent)
try:
    with os.fdopen(fd, 'w') as out: out.write(edit['after'])
    os.replace(staging, file)
finally:
    if os.path.exists(staging): os.unlink(staging)
"#;

fn set_guest_mode(info: &super::BundleInfo) -> Result<()> {
    use std::io::Write;
    let output = super::guest_shell(info)?.args(["python3", "-c", READ_CONFIG]).output()?;
    if !output.status.success() {
        bail!("Could not read the VM host's config; retry this step");
    }
    let mut edit: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("reading the VM config response")?;
    let before = edit["before"].as_str().unwrap_or("");
    let after = atomics_core::selfconfig::edit(before, "[ui]\nmode = \"simple\"\n", &[]).map_err(anyhow::Error::msg)?;
    edit["after"] = after.into();
    let mut child = super::guest_shell(info)?.args(["python3", "-c", WRITE_CONFIG]).stdin(Stdio::piped()).spawn()?;
    child.stdin.take().context("VM config input")?.write_all(&serde_json::to_vec(&edit)?)?;
    if !child.wait()?.success() {
        bail!("Could not save simple mode on the VM host; retry this step");
    }
    Ok(())
}

/// Stream the latest output into the setup row, retaining bounded failure details. Only this child's process group
/// is cancelled; Lima instances are never deleted or replaced by cancellation or retry.
pub fn run(mut command: Command, stop: &AtomicBool, progress: &dyn Fn(String)) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let output = tempfile::NamedTempFile::new()?;
    let writer = output.reopen()?;
    let temporary = tempfile::tempdir()?;
    let mut child = command
        .env("TMPDIR", temporary.path())
        .stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer)
        .process_group(0)
        .spawn()
        .context("starting the setup command")?;
    let started = Instant::now();
    let mut previous = String::new();
    loop {
        // Once the child has exited, the log includes its final writes.
        let status = child.try_wait()?;
        let mut reader = output.reopen()?;
        let length = reader.metadata()?.len();
        reader.seek(SeekFrom::Start(length.saturating_sub(8192)))?;
        let mut bytes = Vec::new();
        reader.take(8192).read_to_end(&mut bytes)?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if text != previous {
            progress(text.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("").to_string());
            previous = text;
        }
        if let Some(status) = status {
            if !status.success() {
                bail!("Setup command failed ({status}):\n{previous}");
            }
            return Ok(());
        }
        if stop.load(Ordering::Relaxed) || started.elapsed() > Duration::from_secs(2 * 60 * 60) {
            // SAFETY: the child was put in a new group whose id is its pid, and is still running.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            bail!("Setup stopped. You can retry this step.");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_are_independent_of_signins_and_full_remains_the_default() {
        let mut state = State::default();
        assert!(!state.selected);
        assert_eq!(state.info().steps.len(), 6);
        assert!(state.ready(CheckVmRequirements).is_ok());
        for action in [DownloadVm, CreateVm, CheckVmLogins, ConnectVm, SimpleMode] {
            assert!(state.ready(action).is_err());
        }
        state.done = vec![CheckVmRequirements, DownloadVm, CreateVm];
        assert!(state.ready(ConnectVm).is_ok(), "sign-ins can be left for later");
        assert!(state.ready(VmLogin).is_ok());
        state.done.push(ConnectVm);
        assert!(state.ready(SimpleMode).is_ok());
        state.download = Some("/scratch/verified-vm".into());
        let create = state.command(CreateVm, Path::new("/fixture/atomics")).unwrap();
        assert_eq!(
            create.get_args().collect::<Vec<_>>(),
            ["vm", "setup", "--simple", "--from", "/scratch/verified-vm"]
        );
        assert!(!create.get_args().any(|arg| arg == "--replace"));
    }

    #[test]
    fn a_failed_step_has_progress_and_output_and_can_be_retried() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("step");
        crate::test_support::write_program(
            &script,
            "#!/bin/sh\necho 'verifying image'\necho 'damaged image' >&2\nexit 7\n",
        );
        let messages = std::cell::RefCell::new(Vec::new());
        let error =
            run(Command::new(&script), &AtomicBool::new(false), &|text| messages.borrow_mut().push(text)).unwrap_err();
        assert!(error.to_string().contains("damaged image"), "{error:#}");
        assert!(!messages.borrow().is_empty());
        crate::test_support::write_program(&script, "#!/bin/sh\necho 'verified'\n");
        run(Command::new(&script), &AtomicBool::new(false), &|_| {}).unwrap();
    }

    #[test]
    fn cancelling_a_step_is_bounded_and_retryable() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 60"]);
        let started = Instant::now();
        assert!(run(command, &AtomicBool::new(true), &|_| {}).unwrap_err().to_string().contains("retry"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn guest_mode_uses_the_config_editor_and_preserves_other_settings() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("vm.toml");
        let before = "# Keep this\n[chief]\nwake_secs = 90\n[ui]\nmode = \"full\"\n";
        std::fs::write(&file, before).unwrap();
        let after = atomics_core::selfconfig::edit(before, "[ui]\nmode = \"simple\"", &[]).unwrap();
        let edit = serde_json::json!({"file": file, "before": before, "after": after});
        let write = |edit: &serde_json::Value| {
            use std::io::Write;
            let mut child = Command::new("python3")
                .args(["-c", WRITE_CONFIG])
                .stdin(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(&serde_json::to_vec(edit).unwrap()).unwrap();
            child.wait().unwrap().success()
        };
        assert!(write(&edit));
        let saved = std::fs::read_to_string(&file).unwrap();
        assert!(
            saved.contains("# Keep this") && saved.contains("wake_secs = 90") && saved.contains("mode = \"simple\"")
        );
        assert!(!write(&edit), "a concurrent edit must be kept");
        assert_eq!(std::fs::read_to_string(file).unwrap(), saved);
    }
}
