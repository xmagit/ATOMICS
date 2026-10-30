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
}

#[derive(Clone, Copy)]
pub enum Resource {
    Sandbox,
    Mcp,
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

/// Record only after a successful create/add, never by adopting an inventory entry.
pub fn created(home: &Path, name: &str, resource: Resource) -> Result<()> {
    let mut ledger = Ledger::open(home)?;
    let entry = ledger
        .entries
        .entry(name.into())
        .or_insert_with(|| Owned { created: timestamp(SystemTime::now()), ..Owned::default() });
    match resource {
        Resource::Sandbox => entry.sandbox = true,
        Resource::Mcp => entry.mcp = true,
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
        }
        if !entry.sandbox && !entry.mcp {
            ledger.entries.remove(name);
        }
        ledger.save()?;
    }
    Ok(())
}
