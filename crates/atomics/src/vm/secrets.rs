//! Mac keychain -> VM administrator -> broker. The host protocol never carries these values.
//! The journal contains names only, so an offline removal can be retried after the window restarts.
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use atomics_core::secrets::Secret;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::BundleInfo;
use crate::credentials::Store;

pub const ONE_TRUST_DOMAIN: &str = "One trust domain: this VM has no separate administrator. Secrets sync through the VM user's sudo; agents can become root and read the broker's store. Enable atomics.vm.hardened for separation.";

pub trait SecretSource {
    fn values(&self) -> Result<Vec<(Secret, Zeroizing<String>)>>;
}
impl SecretSource for Store {
    fn values(&self) -> Result<Vec<(Secret, Zeroizing<String>)>> {
        self.secret_values()
    }
}

pub trait Broker {
    fn set(&self, spec: &Secret, value: &str) -> Result<()>;
    fn remove(&self, name: &str) -> Result<()>;
}

/// Each mutation selects its account under the journal lock and keeps that
/// verified SSH connection through the write. A reboot breaks the stream.
pub(crate) struct AdminBroker<F>(pub F);
impl<F: Fn(&[u8]) -> Result<()>> Broker for AdminBroker<F> {
    fn set(&self, spec: &Secret, value: &str) -> Result<()> {
        (self.0)(&Zeroizing::new(serde_json::to_vec(&serde_json::json!({"op":"set", "spec":spec, "value":value}))?))
    }
    fn remove(&self, name: &str) -> Result<()> {
        (self.0)(&serde_json::to_vec(&serde_json::json!({"op":"remove", "name":name}))?)
    }
}

pub(crate) fn send_verified(
    info: &BundleInfo,
    path: &Path,
    bytes: &[u8],
    mut open: impl FnMut(&BundleInfo) -> Result<super::account::Connection>,
) -> Result<()> {
    let (_, connection) = super::built::BootAccounts::load(path)?.select(path, info, |candidate| {
        let connection = open(candidate)?;
        Ok((connection.system.clone(), connection))
    })?;
    connection.send(bytes)
}

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct Report {
    pub rows: BTreeMap<String, String>,
    pub removals: BTreeMap<String, String>,
}
impl Report {
    pub fn check(&self) -> Result<()> {
        if self.rows.values().chain(self.removals.values()).any(|state| state != "Synced") {
            bail!("Keychain saved; VM secret sync failed. Reconnect or refresh credentials to retry");
        }
        Ok(())
    }
}

/// Persist intent before sending anything. Only names previously managed by this Mac are removed;
/// broker entries installed independently by an administrator are left alone.
fn reconcile(
    source: &impl SecretSource,
    broker: &impl Broker,
    known: &mut BTreeSet<String>,
    mut save: impl FnMut(&BTreeSet<String>) -> Result<()>,
) -> Result<Report> {
    let values = source.values()?;
    let present: BTreeSet<_> = values.iter().map(|(spec, _)| spec.name.clone()).collect();
    known.extend(present.iter().cloned());
    save(known)?;
    let mut report = Report::default();
    for name in known.difference(&present).cloned().collect::<Vec<_>>() {
        match broker.remove(&name) {
            Ok(()) => {
                known.remove(&name);
            }
            Err(error) => {
                report.removals.insert(name, format!("Failed: {error}"));
            }
        }
    }
    // List exposes rules, not a value revision or proof of the broker's current value. A saved
    // Mac acknowledgement cannot detect replacement brokers or independent admin changes.
    // Resend for correctness; the broker's no-op path preserves grants and DNS configuration.
    for (spec, value) in values {
        let state = match broker.set(&spec, &value) {
            Ok(()) => "Synced".into(),
            Err(error) => format!("Failed: {error}"),
        };
        report.rows.insert(spec.name, state);
    }
    save(known)?;
    Ok(report)
}

pub fn sync(installed: &Path, info: &BundleInfo, store: &Store) -> Result<Report> {
    sync_in(&super::update::running_info(installed, info)?, store, &atomics_core::paths::atomics_home().join("vm"))
}

fn sync_in(info: &BundleInfo, store: &Store, dir: &Path) -> Result<Report> {
    let path = super::built::BootAccounts::path(info);
    let broker = AdminBroker(|bytes: &[u8]| {
        send_verified(info, &path, bytes, |candidate| super::account::Connection::open(candidate, true))
    });
    sync_with(&info.instance, store, &broker, dir)
}

pub(crate) fn sync_with(
    instance: &str,
    source: &impl SecretSource,
    broker: &impl Broker,
    dir: &Path,
) -> Result<Report> {
    anyhow::ensure!(super::valid_instance(instance), "Invalid VM instance");
    std::fs::create_dir_all(dir)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(dir.join(format!("{instance}-secrets.lock")))?;
    lock.lock().context("locking VM secret sync")?;
    let file = dir.join(format!("{instance}-secrets.json"));
    let mut known: BTreeSet<String> = match std::fs::read(&file) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("Invalid VM secret sync journal (names only)")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
        Err(error) => return Err(error.into()),
    };
    reconcile(source, broker, &mut known, |names| {
        let mut next = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer(&mut next, names)?;
        next.as_file().sync_all()?;
        next.persist(&file).map_err(|_| anyhow::anyhow!("Could not save VM secret sync journal"))?;
        Ok(())
    })
}

/// CLI and VM lifecycle entry point. Native windows use `sync` on a credential worker.
pub fn push(installed: &Path, info: &BundleInfo) -> Result<()> {
    let info = super::built::running_bundle(installed, info)?;
    if info.admin.is_none() {
        eprintln!("{ONE_TRUST_DOMAIN}");
    }
    let store = Store::local(crate::relay::Keychain::current());
    let report = sync_in(&info, &store, &atomics_core::paths::atomics_home().join("vm"))?;
    for (name, state) in report.rows.iter().chain(&report.removals) {
        if state != "Synced" {
            eprintln!("VM secret {name}: {state}");
        }
    }
    report.check()
}

/// Only a Mac configured as a VM client sends CLI mutations to its VM.
pub fn configured() -> Result<Option<BundleInfo>> {
    if !is_vm_client()? {
        return Ok(None);
    }
    Ok(Some(
        BundleInfo::read(&super::installed_dir())
            .context("VM client has no readable VM bundle; run atomics vm setup")?,
    ))
}

pub fn is_vm_client() -> Result<bool> {
    if !cfg!(target_os = "macos") {
        return Ok(false);
    }
    config_is_vm_client(&atomics_core::paths::config_file())
}

fn config_is_vm_client(file: &std::path::Path) -> Result<bool> {
    let config = atomics_core::config::Config::load_existing(file)?;
    Ok(config.client.connect.trim() == "vm")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[test]
    fn missing_client_config_uses_defaults_without_creating_a_file() {
        let dir = crate::test_support::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        assert!(!config_is_vm_client(&file).unwrap());
        assert!(!file.exists());
        std::fs::write(&file, "[client]\nconnect = 'vm'\n").unwrap();
        assert!(config_is_vm_client(&file).unwrap());
        std::fs::write(&file, "[client]\nconnect = 'local'\n").unwrap();
        assert!(!config_is_vm_client(&file).unwrap());
        std::fs::write(&file, "invalid = [").unwrap();
        assert!(config_is_vm_client(&file).is_err());
    }

    #[derive(Default)]
    struct FakeBroker {
        fail: Cell<bool>,
        values: RefCell<BTreeMap<String, String>>,
    }
    impl Broker for FakeBroker {
        fn set(&self, spec: &Secret, value: &str) -> Result<()> {
            if self.fail.get() {
                bail!("VM unavailable");
            }
            self.values.borrow_mut().insert(spec.name.clone(), value.into());
            Ok(())
        }
        fn remove(&self, name: &str) -> Result<()> {
            if self.fail.get() {
                bail!("VM unavailable");
            }
            self.values.borrow_mut().remove(name);
            Ok(())
        }
    }

    #[test]
    fn reconnect_resends_values_and_retries_removals_from_names_only_intent() {
        let (_dir, _, store) = crate::credentials::tests::machine(false);
        let spec = Secret { name: "KEY".into(), ..Default::default() };
        store.add_secret(spec.clone(), "canary-sync").unwrap();
        let broker = FakeBroker::default();
        broker.values.borrow_mut().insert("INDEPENDENT_ADMIN_KEY".into(), "independent".into());
        let mut known = BTreeSet::new();
        let saved = RefCell::new(Vec::new());
        let save = |names: &BTreeSet<String>| {
            saved.borrow_mut().push(serde_json::to_string(names).unwrap());
            Ok(())
        };
        broker.fail.set(true);
        let failed = reconcile(&store, &broker, &mut known, save).unwrap();
        assert_eq!(failed.rows["KEY"], "Failed: VM unavailable");
        assert!(failed.check().is_err());
        assert_eq!(saved.borrow()[0], "[\"KEY\"]", "intent is saved before any send");
        broker.fail.set(false);
        assert_eq!(reconcile(&store, &broker, &mut known, save).unwrap().rows["KEY"], "Synced");
        // An empty/new broker is repopulated on reconnect, even after a previous acknowledgement.
        broker.values.borrow_mut().remove("KEY");
        reconcile(&store, &broker, &mut known, save).unwrap().check().unwrap();
        assert_eq!(broker.values.borrow()["KEY"], "canary-sync");
        store.remove_secret("KEY").unwrap();
        broker.fail.set(true);
        let removed = reconcile(&store, &broker, &mut known, save).unwrap();
        assert_eq!(removed.removals["KEY"], "Failed: VM unavailable");
        assert!(known.contains("KEY"));
        // Simulate a fresh window loading the durable names-only journal.
        let mut restored = serde_json::from_str(saved.borrow().last().unwrap()).unwrap();
        broker.fail.set(false);
        reconcile(&store, &broker, &mut restored, save).unwrap().check().unwrap();
        assert!(restored.is_empty());
        assert!(!broker.values.borrow().contains_key("KEY"));
        assert!(broker.values.borrow().contains_key("INDEPENDENT_ADMIN_KEY"));
        assert!(!format!("{failed:?} {removed:?} {:?}", saved.borrow()).contains("canary"));
    }

    #[test]
    fn persisted_removal_survives_a_new_sync_worker_and_journal_has_no_values() {
        let (dir, _, store) = crate::credentials::tests::machine(false);
        let journal = dir.path().join("sync");
        let broker = FakeBroker::default();
        store.add_secret(Secret { name: "KEY".into(), ..Default::default() }, "canary-journal").unwrap();
        sync_with("vm", &store, &broker, &journal).unwrap().check().unwrap();
        store.remove_secret("KEY").unwrap();
        broker.fail.set(true);
        assert!(sync_with("vm", &store, &broker, &journal).unwrap().check().is_err());
        assert_eq!(std::fs::read_to_string(journal.join("vm-secrets.json")).unwrap(), "[\"KEY\"]");
        broker.fail.set(false);
        sync_with("vm", &store, &broker, &journal).unwrap().check().unwrap();
        assert!(!broker.values.borrow().contains_key("KEY"));
        assert_eq!(std::fs::read_to_string(journal.join("vm-secrets.json")).unwrap(), "[]");
    }
}
