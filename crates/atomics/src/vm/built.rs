//! Whether the VM runs the system this Mac configures: the `[vm] modules` it was built with, and hardened mode.
//!
//! `atomics vm update` builds the system with a record of those modules, by their paths on the Mac with each
//! file's SHA-256, and of whether it is hardened: `/etc/atomics/vm-build.json`, in the Nix store. The guest's
//! running system (`/run/current-system`) and its next boot (the system profile) each have theirs, which any
//! user can read. The Mac hashes the files `[vm] modules` names now and compares; hardened mode counts as active
//! only when the running system records it, the VM's user is not in `wheel`, and the admin user answers. A
//! difference names its fix: `atomics vm update` (docs/nixos-vm.md, Your own NixOS modules).
//!
//! In hardened mode the guest is asked as the admin user, so nothing an agent writes in the VM's user's home
//! changes the answer. Without it the VM's user may become root, and the VM and this account are one trust
//! domain (docs/nixos-vm.md, Trust and secrets).

use std::io::{Read, Seek, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::{BundleInfo, Status};

/// The command that makes the VM run what this Mac configures.
pub const FIX: &str = "atomics vm update";

/// Where a system built by `atomics vm update` keeps its record, under its own `etc`.
const RECORD: &str = "etc/atomics/vm-build.json";
const RUNNING: &str = "/run/current-system";
const NEXT_BOOT: &str = "/nix/var/nix/profiles/system";
/// The probe's output starts after this line; a login shell may print something first.
const MARKER: &str = "atomics-vm-build";
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_OUTPUT: u64 = 64 * 1024;

/// A system's record of what it was built with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub version: u32,
    pub hardened: bool,
    pub modules: Vec<Digest>,
}

/// One module the system was built with: its path on the Mac and its file's SHA-256, in hex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Digest {
    pub name: String,
    pub sha256: String,
}

/// One `[vm] modules` entry as this Mac resolves it now: its real path, and its file's SHA-256 or why it cannot
/// be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Configured {
    pub name: String,
    pub sha256: Result<String, String>,
}

/// Each `[vm] modules` entry by the real path `atomics vm update` imports it from, hashed as the build hashes it.
pub fn configured(modules: &[String]) -> Vec<Configured> {
    modules
        .iter()
        .map(|module| {
            let path = atomics_core::paths::expand(module);
            match path.canonicalize() {
                Ok(real) => Configured {
                    name: real.display().to_string(),
                    sha256: std::fs::read(&real).map(|bytes| sha256(&bytes)).map_err(|error| error.to_string()),
                },
                Err(error) => Configured { name: path.display().to_string(), sha256: Err(error.to_string()) },
            }
        })
        .collect()
}

fn sha256(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes).as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// What the guest says of itself, read without root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guest {
    /// The running system's record; `None` for a system built before records, or from the image.
    pub running: Option<Record>,
    /// The next boot's record: differs from `running` after an update that did not restart.
    pub next_boot: Option<Record>,
    /// The VM's user is in `wheel`, so it may use sudo.
    pub wheel: bool,
    /// The bundle's admin user answered, over its own ssh connection.
    pub admin: bool,
}

/// The guest's side, in a shell of the user named: the two records, then the VM's user's groups.
fn probe_script(user: &str, running: &str, next_boot: &str) -> String {
    let quote = super::update::quote;
    format!(
        "printf '%s\\n' {MARKER}\nfor record in {} {}; do\n  if [ -f \"$record\" ]; then tr -d '\\n' < \"$record\"; else printf null; fi\n  printf '\\n'\ndone\nid -nG {}\n",
        quote(&format!("{running}/{RECORD}")),
        quote(&format!("{next_boot}/{RECORD}")),
        quote(user)
    )
}

fn parse_guest(output: &str, admin: bool) -> Result<Guest> {
    let lines: Vec<&str> = output.lines().collect();
    let start = lines.iter().rposition(|line| line.trim() == MARKER).context("the VM's answer has no build record")?;
    let [running, next_boot, groups] = &lines[start + 1..] else {
        bail!("the VM's answer about its build is not three lines")
    };
    let record = |line: &str, which: &str| -> Result<Option<Record>> {
        let record: Option<Record> =
            serde_json::from_str(line).with_context(|| format!("reading the {which} system's build record"))?;
        if let Some(record) = &record
            && record.version != 1
        {
            bail!("the {which} system's build record is version {}; this ATOMICS reads version 1", record.version);
        }
        Ok(record)
    };
    Ok(Guest {
        running: record(running, "running")?,
        next_boot: record(next_boot, "next boot's")?,
        wheel: groups.split_whitespace().any(|group| group == "wheel"),
        admin,
    })
}

/// Where this host runs, as it can tell from inside: ATOMICS's VM when its running system carries the build record,
/// with whether hardened mode is active there (the record says so and this process is not in `wheel`); a Mac; or
/// another machine. Read once: none of it changes while the host runs.
pub(crate) fn this_machine() -> (atomics_core::projection::AgentMachine, Option<String>, Option<bool>) {
    static MACHINE: std::sync::LazyLock<(atomics_core::projection::AgentMachine, Option<String>, Option<bool>)> =
        std::sync::LazyLock::new(|| {
            if cfg!(target_os = "macos") {
                return (atomics_core::projection::AgentMachine::Mac, None, None);
            }
            let record = std::fs::read(std::path::Path::new(RUNNING).join(RECORD)).ok();
            let groups = std::fs::read_to_string("/etc/group").unwrap_or_default();
            machine_from(record.as_deref(), &groups, &process_groups(), atomics_core::paths::host_name())
        });
    MACHINE.clone()
}

/// [`this_machine`] on Linux, from the running system's build record, `/etc/group`, this process's groups and its
/// host name.
fn machine_from(
    record: Option<&[u8]>,
    group_file: &str,
    groups: &[libc::gid_t],
    name: String,
) -> (atomics_core::projection::AgentMachine, Option<String>, Option<bool>) {
    use atomics_core::projection::AgentMachine;
    let name = Some(name).filter(|name| !name.trim().is_empty());
    let Some(record) = record.and_then(|bytes| serde_json::from_slice::<Record>(bytes).ok()) else {
        return (AgentMachine::Other, name, None);
    };
    let wheel = group_file.lines().find_map(|line| {
        let mut fields = line.split(':');
        (fields.next() == Some("wheel")).then_some(())?;
        fields.nth(1)?.parse::<libc::gid_t>().ok()
    });
    let in_wheel = wheel.is_some_and(|gid| groups.contains(&gid));
    (AgentMachine::Vm, name, Some(record.hardened && !in_wheel))
}

/// This process's groups, its own and its supplementary ones.
fn process_groups() -> Vec<libc::gid_t> {
    let mut groups = vec![0 as libc::gid_t; 256];
    // SAFETY: `groups` is writable for the length given; getgroups writes at most that many.
    let count = unsafe { libc::getgroups(groups.len() as libc::c_int, groups.as_mut_ptr()) };
    groups.truncate(usize::try_from(count).unwrap_or(0));
    // SAFETY: getegid cannot fail.
    groups.push(unsafe { libc::getegid() });
    groups
}

/// Asks the guest: as the admin user when the bundle has one, else as the VM's user. An admin that does not answer
/// is asked no more; the VM's user then says what it can (an update that has not restarted yet has no admin).
pub fn probe(info: &BundleInfo) -> Result<Guest> {
    probe_with(info, &|info| {
        let script = probe_script(&info.user, RUNNING, NEXT_BOOT);
        output_within(super::system_command(info, &script)?, PROBE_TIMEOUT)
    })
}

fn probe_with(info: &BundleInfo, run: &dyn Fn(&BundleInfo) -> Result<String>) -> Result<Guest> {
    if info.admin.is_some() {
        match run(info) {
            Ok(output) => return parse_guest(&output, true),
            Err(admin) => {
                let user = BundleInfo { admin: None, ..info.clone() };
                let output = run(&user)
                    .with_context(|| format!("asking the VM's user after the admin user did not answer ({admin:#})"))?;
                return parse_guest(&output, false);
            }
        }
    }
    parse_guest(&run(info)?, false)
}

/// Runs `command` for at most `timeout`, keeping at most [`MAX_OUTPUT`] of what it prints. Output goes to unlinked
/// files, so an ssh control master that inherits them cannot keep this waiting.
fn output_within(mut command: Command, timeout: Duration) -> Result<String> {
    let (stdout, stderr) = (tempfile::tempfile()?, tempfile::tempfile()?);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?)
        .spawn()
        .context("asking the VM what it was built with")?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the VM did not answer within {} s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let read = |mut file: std::fs::File| -> Result<String> {
        file.seek(std::io::SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.take(MAX_OUTPUT).read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    };
    if !status.success() {
        let error = read(stderr)?;
        bail!("the VM did not say what it was built with ({status}): {}", error.trim());
    }
    read(stdout)
}

/// How the running system differs from what this Mac configures. Empty when it does not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Drift {
    /// One line per `[vm] module` the running system was not built with as configured.
    pub modules: Vec<String>,
    /// Why hardened mode is not active, when it is configured.
    pub hardened: Option<String>,
    /// The next boot is already what this Mac configures: an update installed it without restarting.
    pub staged: bool,
}

impl Drift {
    pub fn is_empty(&self) -> bool {
        self.modules.is_empty() && self.hardened.is_none()
    }
}

fn module_changes(configured: &[Configured], built: Option<&Record>) -> Vec<String> {
    let Some(built) = built else {
        return if configured.is_empty() {
            Vec::new()
        } else {
            vec![
                "the running system does not record its [vm] modules: it came from the image or an older \
                  ATOMICS, so it may run none of them"
                    .into(),
            ]
        };
    };
    let mut changes = Vec::new();
    for module in configured {
        let name = &module.name;
        match (&module.sha256, built.modules.iter().find(|digest| digest.name == *name)) {
            (Err(error), _) => changes.push(format!("{name}: in [vm] modules, but this Mac cannot read it ({error})")),
            (Ok(_), None) => changes.push(format!("{name}: in [vm] modules, not in the running system")),
            (Ok(sha256), Some(digest)) if *sha256 != digest.sha256 => {
                changes.push(format!("{name}: changed since the running system was built"))
            }
            (Ok(_), Some(_)) => {}
        }
    }
    for digest in built.modules.iter().filter(|digest| !configured.iter().any(|module| module.name == digest.name)) {
        changes.push(format!("{}: in the running system, no longer in [vm] modules", digest.name));
    }
    changes
}

/// Hardened mode counts as configured when the installed bundle was built with it (it has an admin user) or the
/// next boot records it; as active only when the running system records it (or predates records), the VM's user
/// is not in `wheel`, and the bundle's admin user answers.
fn hardened_drift(info: &BundleInfo, guest: &Guest) -> Option<String> {
    let configured = info.admin.is_some() || guest.next_boot.as_ref().is_some_and(|record| record.hardened);
    if !configured {
        return None;
    }
    let why = if guest.wheel {
        format!("its user {} is in wheel and may use sudo without a password", info.user)
    } else if guest.running.as_ref().is_some_and(|record| !record.hardened) {
        "the running system was built without it".into()
    } else if !guest.admin {
        format!("its admin user {} did not answer over Lima's ssh", info.admin.as_deref().unwrap_or("admin"))
    } else {
        return None;
    };
    Some(format!("hardened mode: configured, but not active: {why}"))
}

/// The running system against `configured` and the installed bundle `info`.
pub fn compare(configured: &[Configured], info: &BundleInfo, guest: &Guest) -> Drift {
    let modules = module_changes(configured, guest.running.as_ref());
    let hardened = hardened_drift(info, guest);
    let staged = (!modules.is_empty() || hardened.is_some())
        && guest.next_boot != guest.running
        && guest.next_boot.as_ref().is_some_and(|next| {
            module_changes(configured, Some(next)).is_empty() && (next.hardened || hardened.is_none())
        });
    Drift { modules, hardened, staged }
}

/// What the commands and the app say of the VM's system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// The VM runs what this Mac configures.
    Matches {
        modules: usize,
        hardened: bool,
    },
    Drift(Drift),
    /// Not checked, and why.
    Unknown(String),
}

impl Report {
    pub fn of(configured: &[Configured], info: &BundleInfo, guest: &Guest) -> Self {
        let drift = compare(configured, info, guest);
        if !drift.is_empty() {
            return Report::Drift(drift);
        }
        let hardened = !guest.wheel && guest.running.as_ref().is_none_or(|record| record.hardened);
        Report::Matches { modules: configured.len(), hardened }
    }

    pub fn is_drift(&self) -> bool {
        matches!(self, Report::Drift(_))
    }

    pub fn headline(&self) -> String {
        match self {
            Report::Matches { modules, hardened } => format!(
                "The VM runs the system this Mac configures: {}, hardened mode {}",
                match modules {
                    0 => "no [vm] modules".to_string(),
                    1 => "1 [vm] module".to_string(),
                    n => format!("{n} [vm] modules"),
                },
                if *hardened { "active" } else { "off" }
            ),
            Report::Drift(_) => "The VM does not run the system this Mac configures:".into(),
            Report::Unknown(why) => format!("Not checked whether the VM runs the system this Mac configures: {why}"),
        }
    }

    /// The differences, one per line.
    pub fn details(&self) -> Vec<String> {
        let Report::Drift(drift) = self else { return Vec::new() };
        let mut lines = drift.modules.clone();
        lines.extend(drift.hardened.clone());
        if drift.staged {
            lines.push("its next boot is already that system: an update installed it without restarting".into());
        }
        lines
    }

    /// The command that makes the VM run what this Mac configures, when it does not.
    pub fn fix(&self) -> Option<&'static str> {
        self.is_drift().then_some(FIX)
    }
}

/// The report for the installed bundle `info` and `[vm] modules`, asking the guest with `probe`.
pub fn report_with(info: &BundleInfo, modules: &[String], probe: impl FnOnce(&BundleInfo) -> Result<Guest>) -> Report {
    match probe(info) {
        Ok(guest) => Report::of(&configured(modules), info, &guest),
        Err(error) => Report::Unknown(format!("{error:#}")),
    }
}

/// The report for an explicit command: the guest is asked only while it runs.
pub fn report_now(info: &BundleInfo, modules: &[String]) -> Report {
    match super::status_for_command(&info.instance) {
        Status::Running => report_with(info, modules, probe),
        status => Report::Unknown(format!("the VM {} is {}", info.instance, status.label())),
    }
}

/// Prints `report` as the commands do: its headline, each difference, and the fix.
pub fn say(out: &mut dyn Write, report: &Report) -> std::io::Result<()> {
    writeln!(out, "{}", report.headline())?;
    for line in report.details() {
        writeln!(out, "  {line}")?;
    }
    if let Some(fix) = report.fix() {
        writeln!(out, "Fix: {fix} (builds the system they describe and restarts the VM into it)")?;
    }
    Ok(())
}

/// The check for the app's status polls: the Mac's side at every poll, so an edit of `[vm] modules` shows at once;
/// the guest when it comes up, then at most once a minute (every few seconds while it does not answer).
#[derive(Default)]
pub struct Watch {
    guest: Option<(Instant, Result<Guest, String>)>,
}

const ASK_AGAIN: Duration = Duration::from_secs(60);
const RETRY: Duration = Duration::from_secs(10);

impl Watch {
    /// The report for the installed bundle while the VM runs; `None` otherwise.
    pub fn check(&mut self, status: &Status) -> Option<Report> {
        let installed = BundleInfo::installed().map(|(_, info)| info);
        self.check_with(status, installed, Instant::now(), &probe, &|| {
            super::update::load_config().map(|config| config.vm.modules)
        })
    }

    fn check_with(
        &mut self,
        status: &Status,
        info: Option<BundleInfo>,
        now: Instant,
        probe: &dyn Fn(&BundleInfo) -> Result<Guest>,
        modules: &dyn Fn() -> Result<Vec<String>>,
    ) -> Option<Report> {
        if *status != Status::Running {
            self.guest = None;
            return None;
        }
        let info = info?;
        let due = self.guest.as_ref().is_none_or(|(at, guest)| {
            now.saturating_duration_since(*at) >= if guest.is_ok() { ASK_AGAIN } else { RETRY }
        });
        if due {
            self.guest = Some((now, probe(&info).map_err(|error| format!("{error:#}"))));
        }
        let modules = match modules() {
            Ok(modules) => modules,
            Err(error) => return Some(Report::Unknown(format!("reading [vm] modules: {error:#}"))),
        };
        Some(match &self.guest.as_ref()?.1 {
            Ok(guest) => Report::of(&configured(&modules), &info, guest),
            Err(error) => Report::Unknown(error.clone()),
        })
    }
}

#[cfg(test)]
mod tests;
