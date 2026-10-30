//! sbx is machine-wide. A name prefix is never proof that this IMACS home owns a resource.
use std::collections::BTreeMap;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Owned {
    pub sandbox: bool,
    pub mcp: bool,
    pub created: u64,
    pub pins: Option<PathBuf>,
    pub scratch: Option<PathBuf>,
    pub builder: Option<u32>,
    /// The relay port the sandbox's `localhost:<port>` policy rule allows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay: Option<u16>,
}

#[derive(Clone, Copy)]
pub enum Resource {
    Sandbox,
    Mcp,
    Pins,
    Scratch,
    Builder,
    Relay,
}

pub fn timestamp(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos().try_into().unwrap_or(u64::MAX)
}

struct Ledger {
    // flock also serializes different processes sharing one IMACS_HOME. Closing releases it.
    _lock: File,
    path: PathBuf,
    entries: BTreeMap<String, Owned>,
}

impl Ledger {
    fn entry(&mut self, name: &str) -> &mut Owned {
        self.entries
            .entry(name.into())
            .or_insert_with(|| Owned { created: timestamp(SystemTime::now()), ..Owned::default() })
    }
    fn open(home: &Path) -> Result<Self> {
        std::fs::create_dir_all(home)?;
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(home.join("sandbox-ownership.lock"))?;
        // SAFETY: lock owns the open descriptor for the lifetime of the ledger.
        anyhow::ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0,
            "locking sandbox ownership: {}",
            std::io::Error::last_os_error()
        );
        let path = home.join("sandbox-ownership.json");
        let entries = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("reading sandbox ownership ledger")?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(err) => return Err(err.into()),
        };
        Ok(Self { _lock: lock, path, entries })
    }

    fn save(&self) -> Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&self.entries)?)?;
        std::fs::rename(tmp, &self.path)?;
        Ok(())
    }
}

pub fn owned(home: &Path) -> Result<BTreeMap<String, Owned>> {
    Ok(Ledger::open(home)?.entries)
}

/// Reserve before the create/add starts, so a crash or timeout part way still leaves the resource
/// owned; removal treats one sbx does not list as gone. Never record by adopting an inventory entry.
pub fn created(home: &Path, name: &str, resource: Resource) -> Result<()> {
    let mut ledger = Ledger::open(home)?;
    let entry = ledger.entry(name);
    match resource {
        Resource::Sandbox => entry.sandbox = true,
        Resource::Mcp => entry.mcp = true,
        Resource::Pins | Resource::Scratch | Resource::Builder | Resource::Relay => {
            anyhow::bail!("use the record operation for this resource")
        }
    }
    ledger.save()
}

/// Keep a failed removal recorded so the next sweep can retry it.
pub fn removed(home: &Path, name: &str, resource: Resource) -> Result<()> {
    let mut ledger = Ledger::open(home)?;
    if let Some(entry) = ledger.entries.get_mut(name) {
        match resource {
            Resource::Sandbox => entry.sandbox = false,
            Resource::Mcp => entry.mcp = false,
            Resource::Pins => entry.pins = None,
            Resource::Scratch => entry.scratch = None,
            Resource::Builder => entry.builder = None,
            Resource::Relay => entry.relay = None,
        }
        if !entry.sandbox
            && !entry.mcp
            && entry.pins.is_none()
            && entry.scratch.is_none()
            && entry.builder.is_none()
            && entry.relay.is_none()
        {
            ledger.entries.remove(name);
        }
        ledger.save()?;
    }
    Ok(())
}

/// The sandbox's policy rule now lets it reach the relay at `port`.
pub fn record_relay(home: &Path, name: &str, port: u16) -> Result<()> {
    let mut ledger = Ledger::open(home)?;
    ledger.entry(name).relay = Some(port);
    ledger.save()
}

pub fn record_pins(home: &Path, name: &str, dir: &Path) -> Result<()> {
    let mut ledger = Ledger::open(home)?;
    ledger.entry(name).pins = Some(dir.into());
    ledger.save()
}

/// A live CLI build has no session record; its owning process keeps it out of the sweep.
pub struct TemplateBuild {
    home: PathBuf,
    name: String,
}

impl TemplateBuild {
    pub fn record(home: &Path, name: &str, scratch: &Path) -> Result<Self> {
        let mut ledger = Ledger::open(home)?;
        let entry = ledger.entry(name);
        entry.builder = Some(std::process::id());
        entry.scratch = Some(scratch.into());
        ledger.save()?;
        Ok(Self { home: home.into(), name: name.into() })
    }
}

impl Drop for TemplateBuild {
    fn drop(&mut self) {
        if let Err(error) = removed(&self.home, &self.name, Resource::Builder) {
            eprintln!("IMACS: releasing template build ownership: {error:#}");
        }
    }
}

pub fn building(entry: &Owned) -> bool {
    entry.builder.is_some_and(|pid| unsafe { libc::kill(pid as i32, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}
