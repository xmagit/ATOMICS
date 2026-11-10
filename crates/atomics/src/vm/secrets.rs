//! Mac keychain -> VM administrator -> broker. The host protocol never carries these values.
//! The journal contains names only, so an offline removal can be retried after the window restarts.
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use atomics_core::secrets::Secret;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::BundleInfo;
use crate::credentials::Store;

pub const ONE_TRUST_DOMAIN: &str = "One trust domain: this VM has no separate administrator. Secrets sync through the VM user's sudo; agents can become root and read the broker's store. Enable atomics.vm.hardened for separation.";
const ADMIN: &str = "exec /run/current-system/sw/bin/atomics secret-broker-admin";
const SUDO_ADMIN: &str = "exec sudo -n /run/current-system/sw/bin/atomics secret-broker-admin";

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

/// A command factory also lets Linux tests replace only ssh, keeping the production account,
/// connection-sharing options and remote command intact.
pub struct AdminBroker<F: Fn() -> Result<Command>>(pub F);
impl<F: Fn() -> Result<Command>> AdminBroker<F> {
    fn call(&self, request: serde_json::Value) -> Result<()> {
        let bytes = Zeroizing::new(serde_json::to_vec(&request)?);
        let mut command = (self.0)()?;
        private_stdin(&mut command, &bytes, Duration::from_secs(30))
    }
}
impl<F: Fn() -> Result<Command>> Broker for AdminBroker<F> {
    fn set(&self, spec: &Secret, value: &str) -> Result<()> {
        self.call(serde_json::json!({"op":"set", "spec":spec, "value":value}))
    }
    fn remove(&self, name: &str) -> Result<()> {
        self.call(serde_json::json!({"op":"remove", "name":name}))
    }
}

pub(crate) fn command(info: &BundleInfo) -> Result<Command> {
    super::system_command(info, if info.admin.is_some() { ADMIN } else { SUDO_ADMIN })
}

/// Private input and discarded output: neither a failed ssh nor a helper that echoes input can
/// put a value into a UI error or log. A bounded socket write and process group bound stuck helpers.
fn private_stdin(command: &mut Command, bytes: &[u8], timeout: Duration) -> Result<()> {
    let (mut writer, reader) = std::os::unix::net::UnixStream::pair()?;
    writer.set_write_timeout(Some(timeout))?;
    let mut child = command
        .stdin(Stdio::from(OwnedFd::from(reader)))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|_| anyhow::anyhow!("Could not start VM admin connection; check SSH/Lima installation"))?;
    let written = writer.write_all(bytes);
    drop(writer);
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() && written.is_ok() => return Ok(()),
            Ok(Some(status)) => bail!(
                "VM admin connection or broker refused sync ({status}); check VM availability and broker installation"
            ),
            Ok(None) if written.is_ok() && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                // SAFETY: process_group(0) created this child's private process group.
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.wait();
                bail!("VM admin connection timed out or could not accept input; retry when the VM is reachable");
            }
        }
    }
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

pub fn sync(info: &BundleInfo, store: &Store) -> Result<Report> {
    sync_in(info, store, &atomics_core::paths::atomics_home().join("vm"))
}

fn sync_in(info: &BundleInfo, store: &Store, dir: &Path) -> Result<Report> {
    let broker = AdminBroker(|| command(info));
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
pub fn push(info: &BundleInfo) -> Result<()> {
    if info.admin.is_none() {
        eprintln!("{ONE_TRUST_DOMAIN}");
    }
    let store = Store::local(crate::relay::Keychain::current());
    let report = sync(info, &store)?;
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

    fn info(admin: bool) -> BundleInfo {
        BundleInfo {
            version: 1,
            instance: "atomics-test".into(),
            user: "atomics".into(),
            home: "/home/atomics".into(),
            mac_user: "user".into(),
            install_dir: "/bundle".into(),
            host: Some(super::super::HostInfo { port: 47800 }),
            admin: admin.then(|| "admin".into()),
            build: None,
        }
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
    fn ssh_standin_receives_value_only_on_admin_stdin_and_errors_hide_echoed_input() {
        let dir = crate::test_support::tempdir().unwrap();
        let script = dir.path().join("ssh");
        crate::test_support::write_program(
            &script,
            r#"#!/bin/sh
printf '%s\n' "$@" > "$RECORD/argv"
env > "$RECORD/env"
cat > "$RECORD/stdin"
cat "$RECORD/stdin"
cat "$RECORD/stdin" >&2
exit "$EXIT_CODE"
"#,
        );
        let info = info(true);
        let fail = Cell::new(false);
        let broker = AdminBroker(|| {
            let production = command(&info)?;
            assert_eq!(production.get_program(), "ssh");
            let mut standin = Command::new(&script);
            standin
                .args(production.get_args())
                .env("RECORD", dir.path())
                .env("EXIT_CODE", if fail.get() { "23" } else { "0" });
            Ok(standin)
        });
        let canary = format!("canary-admin-{}\n' $() `echo hidden`", uuid::Uuid::new_v4());
        let spec = Secret { name: "KEY".into(), ..Default::default() };
        broker.set(&spec, &canary).unwrap();
        let input: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("stdin")).unwrap()).unwrap();
        assert_eq!(input["value"], canary);
        let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
        assert!(argv.contains("ControlMaster=no\n") && argv.contains("ControlPath=none\n"), "{argv}");
        assert!(argv.contains("-l\nadmin\nlima-atomics-test\n"), "{argv}");
        assert!(argv.contains("/run/current-system/sw/bin/atomics secret-broker-admin"), "{argv}");
        assert!(!argv.contains("sudo") && !argv.contains("limactl") && !argv.contains(&canary));
        assert!(!std::fs::read_to_string(dir.path().join("env")).unwrap().contains(&canary));
        fail.set(true);
        let error = broker.set(&spec, &canary).unwrap_err().to_string();
        assert!(error.contains("23") && !error.contains(&canary), "{error}");
        fail.set(false);
        broker.remove("KEY").unwrap();
        let removal: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("stdin")).unwrap()).unwrap();
        assert_eq!(removal, serde_json::json!({"op":"remove","name":"KEY"}));
    }

    #[test]
    fn without_admin_the_system_connection_uses_the_vm_users_sudo_and_warns() {
        let dir = crate::test_support::tempdir().unwrap();
        let script = dir.path().join("limactl");
        crate::test_support::write_program(
            &script,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$RECORD/argv\"\ncat > \"$RECORD/stdin\"\n",
        );
        let info = info(false);
        let broker = AdminBroker(|| {
            super::super::system_command_with(&info, SUDO_ADMIN, || {
                let mut guest = Command::new(&script);
                guest.env("RECORD", dir.path()).arg("vm-user");
                Ok(guest)
            })
        });
        broker.set(&Secret { name: "KEY".into(), ..Default::default() }, "canary-sudo").unwrap();
        let args = std::fs::read_to_string(dir.path().join("argv")).unwrap();
        assert_eq!(args, format!("vm-user\nbash\n--login\n-c\n{SUDO_ADMIN}\n"));
        assert!(std::fs::read_to_string(dir.path().join("stdin")).unwrap().contains("canary-sudo"));
        assert!(!args.contains("canary-sudo"));
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

    #[test]
    fn stalled_admin_helpers_are_bounded() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exec sleep 60"]);
        let before = Instant::now();
        let error = private_stdin(&mut command, b"private-canary", Duration::from_millis(50)).unwrap_err();
        assert!(before.elapsed() < Duration::from_secs(2));
        assert!(error.to_string().contains("timed out") && !error.to_string().contains("canary"));
    }
}
