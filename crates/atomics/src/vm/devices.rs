//! The host's view of configured devices and of the devices supplied at boot.
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

use super::update::{MountPlan, Share, plan_mounts, shares_of, writable_folders, writable_overlap};

pub(super) struct Inventory {
    pub peers: Vec<Share>,
    pub configured: Vec<Share>,
    pub running: Option<Vec<Share>>,
    pub boot_id: Option<String>,
    /// A stopped peer has boot data whose former exports cannot all be identified.
    pub unknown_history: bool,
    protected: Vec<PathBuf>,
}

impl Inventory {
    pub fn read(instance: &str) -> Result<Self> {
        let active = super::lima_home();
        let mut inventory = Self {
            peers: Vec::new(),
            configured: Vec::new(),
            running: None,
            boot_id: None,
            unknown_history: false,
            protected: vec![super::record_dir(), atomics_core::paths::atomics_home().join("update.lock")],
        };
        inventory.protected.extend(atomics_core::config::Config::source_paths(&atomics_core::paths::config_file()));
        for home in known_homes()? {
            inventory.protected.push(home.join("_config"));
            inventory.protected.push(home.join("_config/atomics/update.lock"));
            let current_home =
                home == active || home.canonicalize().ok().is_some_and(|p| Some(p) == active.canonicalize().ok());
            if !home.try_exists()? && !current_home {
                continue;
            }
            for vm in super::lima_list_in(&home)? {
                ensure!(super::valid_instance(&vm.name), "Lima reported an invalid instance name");
                let dir = home.join(&vm.name);
                for file in ["atomics", "atomics/mounts.json", "lima.yaml", "cidata.iso"] {
                    inventory.protected.push(dir.join(file));
                }
                let config =
                    vm.config.with_context(|| format!("missing device configuration for {}", dir.display()))?;
                let configured = shares_of(&config.mounts);
                let current = current_home && vm.name == instance;
                let boot = match vm.status.as_str() {
                    "Running" => Some(boot_shares(&dir, &configured).with_context(|| {
                        format!(
                            "cannot establish the live write boundaries of {}; stop it to verify its devices",
                            dir.display()
                        )
                    })?),
                    "Stopped" => {
                        if !current {
                            match boot_shares(&dir, &configured) {
                                Ok((_, shares)) => inventory.peers.extend(shares),
                                Err(_) => inventory.unknown_history = true,
                            }
                        }
                        None
                    }
                    state => bail!("cannot establish the devices of {} while it is {state}", dir.display()),
                };
                if current {
                    inventory.configured = configured;
                    if let Some((id, shares)) = boot {
                        inventory.boot_id = Some(id);
                        inventory.running = Some(shares);
                    }
                } else {
                    inventory.peers.extend(configured);
                    if let Some((_, shares)) = boot {
                        inventory.peers.extend(shares);
                    }
                }
            }
        }
        // Setup can be about to create this instance. Protect its files before its first export.
        if !instance.is_empty() {
            inventory.protected.push(active.join(instance).join("atomics/mounts.json"));
        }
        inventory.check_exports(&inventory.exports())?;
        Ok(inventory)
    }

    pub fn exports(&self) -> Vec<Share> {
        self.peers.iter().chain(&self.configured).chain(self.running.iter().flatten()).cloned().collect()
    }

    /// Reconcile a live mismatch even if a YAML edit already describes the desired next start.
    /// If the running devices match, an outstanding configuration change still needs attention.
    pub fn mount_plan(&self, expected: &[Share]) -> MountPlan {
        if let Some(running) = &self.running {
            let live = plan_mounts(expected, running);
            if !live.changes.is_empty() {
                return live;
            }
        }
        plan_mounts(expected, &self.configured)
    }

    /// Check both existing control files and the ancestors at which missing ones will be created.
    pub fn check_exports(&self, proposed: &[Share]) -> Result<()> {
        let writers = writable_folders(proposed);
        for path in &self.protected {
            if let Some(folder) = writable_overlap(path, &writers) {
                bail!("VM lifecycle state {} overlaps writable share {}", path.display(), folder.display());
            }
        }
        Ok(())
    }
}

fn known_homes() -> Result<Vec<PathBuf>> {
    let mut homes = vec![super::lima_home(), atomics_core::paths::home().join(".lima")];
    let records = super::record_dir();
    for (directory, registry) in [(&records, false), (&home_registry(), true)] {
        if !directory.try_exists()? {
            continue;
        }
        for entry in std::fs::read_dir(directory)? {
            let path = entry?.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let data = atomics_core::git::read_regular_file(&path, 1024 * 1024)?;
            let home = if registry {
                Some(serde_json::from_str::<PathBuf>(&data)?)
            } else {
                // This directory also holds names-only secret journals.
                let json: serde_json::Value = serde_json::from_str(&data)?;
                if json.get("lima_home").is_some() {
                    serde_json::from_value::<super::Record>(json)?.lima_home
                } else {
                    None
                }
            };
            if let Some(home) = home {
                ensure!(home.is_absolute(), "invalid Lima home recorded in {}", path.display());
                homes.push(home);
            }
        }
    }
    for home in &mut homes {
        if home.try_exists()? {
            *home = home.canonicalize()?;
        }
    }
    homes.sort();
    homes.dedup();
    Ok(homes)
}

fn home_registry() -> PathBuf {
    atomics_core::paths::home().join(".lima/_config/atomics/homes")
}

/// Keep custom homes discoverable even after a same-named VM is created in another home.
pub fn remember_home() -> Result<()> {
    let home = super::lima_home();
    if home == atomics_core::paths::home().join(".lima") {
        return Ok(());
    }
    let bytes = serde_json::to_vec(&std::path::absolute(home)?)?;
    let name = format!("{}.json", super::built::sha256(&bytes));
    let path = home_registry().join(name);
    atomics_core::store::write_private(&path, &bytes)?;
    Ok(())
}

/// Lima generates cidata from the effective host configuration at boot. Its virtiofs tags bind
/// the Mac location to the guest mount point. The row's mode survives later edits to lima.yaml.
fn boot_shares(dir: &Path, configured: &[Share]) -> Result<(String, Vec<Share>)> {
    let archive = dir.join("cidata.iso");
    let member = |name: &str| -> Result<String> {
        let output = Command::new("/usr/bin/tar").arg("-xOqf").arg(&archive).arg(name).output()?;
        ensure!(output.status.success(), "cannot read {name} from {}", archive.display());
        Ok(String::from_utf8(output.stdout)?)
    };
    let env = member("lima.env")?;
    let id = env
        .lines()
        .find_map(|line| line.strip_prefix("LIMA_CIDATA_IID="))
        .filter(|id| !id.is_empty())
        .context("missing Lima boot identity")?
        .to_owned();
    let count: usize = env
        .lines()
        .find_map(|line| line.strip_prefix("LIMA_CIDATA_MOUNTS="))
        .context("missing boot device count")?
        .parse()?;
    if count == 0 {
        return Ok((id, vec![]));
    }
    ensure!(env.lines().any(|line| line == "LIMA_CIDATA_MOUNTTYPE=virtiofs"), "unsupported live mount type");
    let data = member("user-data")?;
    let mut devices = Vec::new();
    for line in data.lines().skip_while(|line| *line != "mounts:").skip(1) {
        let Some(row) = line.trim().strip_prefix("- [").and_then(|row| row.strip_suffix(']')) else { break };
        let fields: Vec<_> = row.split(", ").collect();
        let [tag, guest, "virtiofs", options, _, _] = fields.as_slice() else { bail!("unknown boot mount row") };
        if *tag == "vz-rosetta" {
            continue;
        }
        let point = Path::new(guest);
        // A removed same-path mount can still be identified from its tag. A remapped device
        // needs its host location from the configuration and a matching tag from the boot.
        let matches = |location: &Path| {
            let digest = super::built::sha256(format!("{}:{guest}", location.display()).as_bytes());
            tag.strip_prefix("lima-") == Some(&digest[..16])
        };
        let location = configured
            .iter()
            .map(|s| s.location.as_path())
            .chain([point])
            .find(|p| matches(p))
            .context("a boot device no longer has a matching host path")?;
        let writable = match options.trim_matches('"').split(',').next() {
            Some("ro") => false,
            Some("rw") => true,
            _ => bail!("unknown boot device permissions"),
        };
        devices.push(Share { location: location.into(), mount_point: point.into(), writable });
    }
    ensure!(devices.len() == count, "incomplete boot device list in {}", archive.display());
    Ok((id, devices))
}
