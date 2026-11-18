//! Build the guest's next boot and its Mac bundle without replacing the Lima disk.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use atomics_core::config::Config;
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{BuildInfo, BundleInfo, devices::Inventory};

pub struct Update {
    pub yes: bool,
    pub no_restart: bool,
    /// Setup just created this disk and the instance did not exist when setup began.
    pub new_vm: bool,
}

/// Uses mac-build.nix's settings, retaining the installed bundle's identity too. Extra modules are
/// imported at their original location when shared. The system records, by their paths on the Mac, the modules
/// it was built with and whether it is hardened (`super::built`): the Mac compares that with its config.
const EXPRESSION: &str = r#"{ src, settings, modules }:
let
  s = builtins.fromJSON settings;
  extra = map (module: module // {
    file = if builtins.substring 0 1 module.path == "/"
      then builtins.toPath module.path else ./. + ("/" + module.path);
  }) (builtins.fromJSON modules);
  vm = (builtins.getFlake src).nixosConfigurations.atomics-vm.extendModules {
    modules = [ ({ config, lib, ... }: {
      atomics.vm = {
        inherit (s) macUser cpus memoryGiB diskGiB user;
        host.enable = s.host;
        host.port = s.hostPort;
      };
      programs.atomics.macDesktop = {
        installDir = s.installDir;
        lima.instance = s.instance;
      };
      environment.etc."atomics/vm-build.json".text = lib.mkForce (builtins.toJSON {
        version = 1;
        hardened = config.atomics.vm.hardened or false;
        modules = map (module: {
          inherit (module) name;
          sha256 = builtins.hashFile "sha256" module.file;
        }) extra;
      });
    }) ] ++ map (module: module.file) extra;
  };
in {
  toplevel = vm.config.system.build.toplevel;
  bundle = vm.config.programs.atomics.macDesktop.bundle;
  # Not built; `nix eval` shows what the system records.
  record = vm.config.environment.etc."atomics/vm-build.json".text;
}
"#;

#[derive(Debug, Deserialize)]
pub(super) struct LimaConfig {
    cpus: u32,
    memory: String,
    disk: String,
    mounts: Vec<Mount>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Mount {
    location: String,
    mount_point: Option<String>,
    #[serde(default)]
    writable: Option<bool>,
}

/// One Mac folder the VM mounts, as Lima fills a mount in: its Mac path, where the guest sees it, and whether
/// the guest may write it. A read-only mount is a read-only device, which not even root in the guest can
/// mount read-write; the guest's own mount options are only the guest's (docs/nixos-vm.md, Hardened mode).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Share {
    pub(crate) location: PathBuf,
    pub(crate) mount_point: PathBuf,
    pub(crate) writable: bool,
}

impl Mount {
    /// Lima's defaults: the guest path is the Mac path, and a mount is read-only unless it says otherwise.
    fn share(&self) -> Share {
        let location = atomics_core::paths::expand(&self.location);
        let mount_point = self.mount_point.as_deref().map_or_else(|| location.clone(), atomics_core::paths::expand);
        Share { location, mount_point, writable: self.writable.unwrap_or(false) }
    }
}

pub(crate) fn shares_of(mounts: &[Mount]) -> Vec<Share> {
    mounts.iter().map(Mount::share).collect()
}

impl LimaConfig {
    pub(super) fn shares(&self) -> Vec<Share> {
        shares_of(&self.mounts)
    }
    pub(super) fn sizes(&self) -> Result<BuildInfo> {
        let gib = |size: &str| -> Result<u32> {
            size.strip_suffix("GiB")
                .or_else(|| size.strip_suffix('G'))
                .and_then(|n| n.parse().ok())
                .filter(|&n| n > 0)
                .with_context(|| format!("expected a positive whole GiB size in Lima's config, got {size:?}"))
        };
        if self.cpus == 0 {
            bail!("Lima's saved CPU count must be positive");
        }
        Ok(BuildInfo { cpus: self.cpus, memory_gib: gib(&self.memory)?, disk_gib: gib(&self.disk)? })
    }
}

pub(super) fn read_lima(instance: &str) -> Result<LimaConfig> {
    let output = super::limactl_command()?.args(["list", "--format", "{{json .Config}}", instance]).output()?;
    if !output.status.success() {
        bail!("could not read Lima's config: {}", String::from_utf8_lossy(&output.stderr));
    }
    serde_json::from_slice(&output.stdout).context("reading Lima's saved build settings")
}

#[derive(Debug, Deserialize)]
pub(crate) struct Mounts {
    #[serde(default)]
    pub(crate) mounts: Vec<Mount>,
}

/// The shares the bundle's lima.yaml gives an instance: those of the system the bundle was built with. Release
/// bundles write JSON, a YAML subset. A bundle built for a named Mac account writes YAML, which Lima's own
/// parser reads, with an empty Lima home so no default.yaml or override.yaml of this Mac adds to it.
pub(crate) fn bundle_shares(bundle: &Path) -> Result<Vec<Share>> {
    bundle_shares_with(bundle, super::limactl_command)
}

pub(crate) fn bundle_shares_with(bundle: &Path, limactl: impl FnOnce() -> Result<Command>) -> Result<Vec<Share>> {
    let file = bundle.join("lima.yaml");
    let text = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    if let Ok(parsed) = serde_json::from_str::<Mounts>(&text) {
        return Ok(shares_of(&parsed.mounts));
    }
    let empty = tempfile::tempdir()?;
    let mut limactl = limactl()?;
    limactl.env("LIMA_HOME", empty.path());
    yaml_shares(limactl, &file)
}

fn yaml_shares(mut limactl: Command, file: &Path) -> Result<Vec<Share>> {
    let output = limactl
        .args(["template", "yq"])
        .arg(file)
        .arg(r#".mounts // [] | map({"location": .location, "mountPoint": .mountPoint, "writable": .writable}) | to_json(0)"#)
        .stderr(Stdio::piped())
        .output()
        .context("running limactl template yq")?;
    if !output.status.success() {
        bail!("Lima could not read {}: {}", file.display(), String::from_utf8_lossy(&output.stderr).trim());
    }
    let mounts: Vec<Mount> = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("reading the mounts Lima found in {}", file.display()))?;
    Ok(shares_of(&mounts))
}

/// What `limactl edit` changes so the instance mounts exactly the built system's shares, each as writable as the
/// system mounts it, and nothing else of the Mac. Empty when they already match.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct MountPlan {
    /// One line per difference, for the user.
    pub(crate) changes: Vec<String>,
    /// The `limactl edit --set` expression that makes them match.
    pub(crate) set: Option<String>,
}

fn mode(writable: bool) -> &'static str {
    if writable { "writable" } else { "read-only" }
}

pub(crate) fn plan_mounts(built: &[Share], instance: &[Share]) -> MountPlan {
    let mut changes = Vec::new();
    for share in built {
        let path = share.location.display();
        match instance.iter().find(|mount| mount.location == share.location) {
            None => changes.push(format!("{path}: not shared by Lima; add it, {}", mode(share.writable))),
            Some(mount) if mount.mount_point != share.mount_point => changes.push(format!(
                "{path}: Lima mounts it at {} in the guest; mount it at {}",
                mount.mount_point.display(),
                share.mount_point.display()
            )),
            Some(mount) if mount.writable && !share.writable => changes.push(format!(
                "{path}: read-only in the system but writable in Lima, so root in the VM could write it; make it read-only"
            )),
            Some(mount) if !mount.writable && share.writable => {
                changes.push(format!("{path}: writable in the system but read-only in Lima; make it writable"))
            }
            Some(_) => {}
        }
    }
    for mount in instance.iter().filter(|mount| !built.iter().any(|share| share.location == mount.location)) {
        changes.push(format!(
            "{}: shared by Lima ({}) but not by the system; remove it",
            mount.location.display(),
            mode(mount.writable)
        ));
    }
    if changes.is_empty() && (instance.len() != built.len() || instance.iter().any(|mount| !built.contains(mount))) {
        changes.push("Lima lists a share twice; list each once".into());
    }
    let set = (!changes.is_empty()).then(|| format!(".mounts = {}", serde_json::to_string(built).unwrap_or_default()));
    MountPlan { changes, set }
}

/// Makes the stopped instance mount exactly `built`, then reads Lima's config again to be sure: Lima adds the
/// mounts in ~/.lima/_config/default.yaml and override.yaml to every instance, which an edit cannot remove.
pub(crate) fn apply_mounts(instance: &str, built: &[Share]) -> Result<()> {
    apply_mounts_with(
        built,
        || Ok(read_lima(instance)?.shares()),
        |set| {
            let status = super::limactl_command()?
                .args(["edit", "--tty=false", "--set", set, instance])
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .context("running limactl edit")?;
            if !status.success() {
                bail!("limactl edit {instance} failed; its mounts are unchanged");
            }
            Ok(())
        },
    )
}

fn apply_mounts_with(
    built: &[Share],
    read: impl Fn() -> Result<Vec<Share>>,
    edit: impl FnOnce(&str) -> Result<()>,
) -> Result<()> {
    let Some(set) = plan_mounts(built, &read()?).set else { return Ok(()) };
    edit(&set)?;
    let left = plan_mounts(built, &read()?).changes;
    if !left.is_empty() {
        bail!(
            "Lima's mounts still differ from the system's shares after the edit:\n  {}\nA mount in \
             ~/.lima/_config/default.yaml or override.yaml is added to every instance; remove it there",
            left.join("\n  ")
        );
    }
    Ok(())
}

/// The selected system's devices and login information live beside its disk. Older policies
/// contain just a list of shares. Only setup of a new disk or an update establishes its identity.
#[derive(Clone, serde::Serialize, Deserialize)]
pub(super) struct Policy {
    pub shares: Vec<Share>,
    pub identity: Option<Identity>,
}

#[derive(Clone, serde::Serialize, Deserialize)]
pub(super) struct Identity {
    pub bundle: PathBuf,
    pub current_admin: Option<String>,
    pub selected_admin: Option<String>,
    /// Lima regenerates cidata on each start, including starts outside ATOMICS.
    pub staged_from_boot: Option<String>,
}

impl Identity {
    pub fn new(bundle: &Path, info: &BundleInfo) -> Result<Self> {
        Ok(Self {
            bundle: bundle.canonicalize()?,
            current_admin: info.admin.clone(),
            selected_admin: info.admin.clone(),
            staged_from_boot: None,
        })
    }
}

fn policy_path(instance: &str) -> PathBuf {
    super::lima_home().join(instance).join("atomics/mounts.json")
}

impl Policy {
    pub fn saved(instance: &str, inventory: &Inventory) -> Result<Option<Self>> {
        use std::os::unix::fs::MetadataExt;
        let path = policy_path(instance);
        let meta = match path.symlink_metadata() {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        anyhow::ensure!(meta.is_file() && meta.nlink() == 1, "expected a private mount policy at {}", path.display());
        let data = atomics_core::git::read_regular_file(&path, 1024 * 1024)?;
        let mut policy: Self = if data.trim_start().starts_with('[') {
            Self { shares: serde_json::from_str(&data)?, identity: None }
        } else {
            serde_json::from_str(&data).with_context(|| format!("reading {}", path.display()))?
        };
        inventory.check_exports(&policy.shares)?;
        if let Some(identity) = &mut policy.identity
            && let (Some(staged), Some(running)) = (&identity.staged_from_boot, &inventory.boot_id)
            && staged != running
        {
            identity.current_admin.clone_from(&identity.selected_admin);
            identity.staged_from_boot = None;
        }
        Ok(Some(policy))
    }

    pub fn for_start(instance: &str, installed: &Path, inventory: &Inventory) -> Result<Self> {
        if let Some(saved) = Self::saved(instance, inventory)? {
            return Ok(saved);
        }
        let writers = inventory.exports();
        refuse_writable(&[], "", installed.to_str().context("invalid bundle path")?, &writable_folders(&writers))?;
        if BundleInfo::read(installed)?.instance != instance {
            bail!("the installed bundle does not belong to {instance}; cannot verify its shares");
        }
        let mut shares = common_shares(&bundle_shares(installed)?, &inventory.configured);
        if let Some(running) = &inventory.running {
            shares = common_shares(&shares, running);
        }
        Ok(Self { shares, identity: None })
    }

    pub fn save(&self, instance: &str, inventory: &Inventory) -> Result<()> {
        inventory.check_exports(&self.shares)?;
        atomics_core::store::write_private(&policy_path(instance), &serde_json::to_vec(self)?)
            .with_context(|| format!("saving the Lima mount policy for {instance}"))
    }

    fn running_info(&self, installed: &Path, info: &BundleInfo) -> Result<BundleInfo> {
        let mut running = info.clone();
        if let Some(identity) = &self.identity {
            anyhow::ensure!(
                identity.bundle == installed.canonicalize()?,
                "this disk belongs to the bundle in {}",
                identity.bundle.display()
            );
            running.admin.clone_from(&identity.current_admin);
        }
        Ok(running)
    }
}

/// Administrative commands can run between bundle installation and the selected boot.
pub(super) fn running_info(info: &BundleInfo) -> Result<BundleInfo> {
    if policy_path(&info.instance).try_exists()? {
        let inventory = Inventory::read(&info.instance)?;
        if let Some(policy) = Policy::saved(&info.instance, &inventory)? {
            return policy.running_info(&super::installed_dir(), info);
        }
    }
    Ok(info.clone())
}

/// Permissions common to both systems while an update changes the selected guest boot.
/// An interrupted switch may leave either system selected.
pub(super) fn common_shares(first: &[Share], second: &[Share]) -> Vec<Share> {
    first
        .iter()
        .filter_map(|share| {
            let other = second
                .iter()
                .find(|other| other.location == share.location && other.mount_point == share.mount_point)?;
            Some(Share { writable: share.writable && other.writable, ..share.clone() })
        })
        .collect()
}

/// The Mac folders a share lets the VM write, and where they resolve: never a source of code or configuration
/// for this Mac (docs/nixos-vm.md, The exchange folder).
pub(crate) fn writable_folders(shares: &[Share]) -> Vec<PathBuf> {
    let mut folders: Vec<PathBuf> = Vec::new();
    for share in shares.iter().filter(|share| share.writable) {
        for folder in [Some(share.location.clone()), share.location.canonicalize().ok()].into_iter().flatten() {
            if !folders.contains(&folder) {
                folders.push(folder);
            }
        }
    }
    folders
}

/// A relative path on this filesystem, including case aliases on APFS.
fn relative_to<'a>(path: &'a Path, root: &Path) -> Option<&'a Path> {
    use std::os::unix::fs::MetadataExt;
    if let Ok(relative) = path.strip_prefix(root) {
        return Some(relative);
    }
    let meta = root.metadata().ok()?;
    path.ancestors().find_map(|ancestor| {
        let other = ancestor.metadata().ok()?;
        (meta.dev() == other.dev() && meta.ino() == other.ino()).then(|| path.strip_prefix(ancestor).unwrap())
    })
}

/// Follow each link in order. A link under a writable ancestor is replaceable even when
/// its final target is outside every export. Missing install and policy files have parents too.
pub(crate) fn writable_folder_of(path: &Path, folders: &[PathBuf]) -> Option<PathBuf> {
    let mut path = path.to_path_buf();
    for _ in 0..64 {
        if let Some(folder) = folders.iter().find(|folder| relative_to(&path, folder).is_some()) {
            return Some(folder.clone());
        }
        let (link, target) =
            path.ancestors().filter_map(|part| part.read_link().ok().map(|target| (part, target))).last()?;
        path = link.parent()?.join(target).join(path.strip_prefix(link).ok()?);
    }
    // An unresolved link cycle cannot establish containment.
    folders.first().cloned()
}

/// An exported directory can contain the trusted tree or expose just one of its subtrees.
pub(super) fn writable_overlap(tree: &Path, folders: &[PathBuf]) -> Option<PathBuf> {
    writable_folder_of(tree, folders).or_else(|| {
        let trusted = [tree.to_path_buf()];
        folders.iter().find(|folder| writable_folder_of(folder, &trusted).is_some()).cloned()
    })
}

fn git_dirs(checkout: &Path) -> Result<[PathBuf; 2]> {
    let dirs =
        atomics_core::git::git(checkout, &["rev-parse", "--path-format=absolute", "--git-dir", "--git-common-dir"])?;
    let dirs: Vec<_> = dirs.lines().map(PathBuf::from).collect();
    dirs.try_into()
        .ok()
        .filter(|paths: &[PathBuf; 2]| paths.iter().all(|p| p.is_absolute()))
        .context("Git did not identify both metadata directories")
}

/// The git folder a checkout's `.git` leads to: a link, or a file naming it (a worktree).
fn git_folder(checkout: &Path) -> Option<PathBuf> {
    let dot_git = checkout.join(".git");
    let meta = std::fs::symlink_metadata(&dot_git).ok()?;
    if meta.is_file() {
        let text = atomics_core::git::read_regular_file(&dot_git, 4096).ok()?;
        let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
        return Some(checkout.join(target));
    }
    Some(dot_git)
}

/// `[vm] modules`, `[update] source` and the bundle's install folder never lie in a folder the VM can write:
/// the VM's system is built from the first two, and the Mac runs the bundle's programs from the last.
pub(crate) fn refuse_writable(modules: &[String], source: &str, install_dir: &str, writable: &[PathBuf]) -> Result<()> {
    let refuse = |what: String, path: &Path, folder: PathBuf| -> Result<()> {
        bail!(
            "{what} {} overlaps {}, a folder the VM can write. ATOMICS on this Mac never takes code from a folder \
             the VM can write: move it to a read-only share such as ~/repos, or outside the shares \
             (docs/nixos-vm.md, The exchange folder)",
            path.display(),
            folder.display()
        )
    };
    for module in modules {
        let path = atomics_core::paths::expand(module);
        if let Some(folder) = writable_folder_of(&path, writable) {
            refuse("[vm] modules:".into(), &path, folder)?;
        }
    }
    if !source.trim().is_empty() {
        let checkout = atomics_core::paths::expand(source);
        let git = git_folder(&checkout);
        let common = git.as_ref().and_then(|dir| {
            atomics_core::git::read_regular_file(&dir.join("commondir"), 4096).ok().map(|path| dir.join(path.trim()))
        });
        for path in [Some(checkout.clone()), git, common].into_iter().flatten() {
            if let Some(folder) = writable_overlap(&path, writable) {
                refuse("[update] source:".into(), &path, folder)?;
            }
        }
        // Git can discover a parent repository or follow a worktree's indirection.
        if let Ok(dirs) = git_dirs(&checkout) {
            for dir in dirs {
                if let Some(folder) = writable_overlap(&dir, writable) {
                    refuse("[update] source:".into(), &dir, folder)?;
                }
            }
        }
    }
    let install = PathBuf::from(install_dir);
    if let Some(folder) = writable_overlap(&install, writable) {
        refuse("The bundle's install folder".into(), &install, folder)?;
    }
    Ok(())
}

/// Settings present in old bundles' Lima definition, now also saved in atomics-vm.json.
fn build_info(info: &BundleInfo, lima: &LimaConfig) -> Result<BuildInfo> {
    let build = match &info.build {
        Some(build) => build.clone(),
        None => lima.sizes()?,
    };
    if build.cpus == 0 || build.memory_gib == 0 || build.disk_gib == 0 {
        bail!("the bundle has invalid VM build settings");
    }
    Ok(build)
}

fn settings(info: &BundleInfo, build: &BuildInfo) -> Value {
    json!({
        "macUser": info.mac_user, "user": info.user, "instance": info.instance,
        "cpus": build.cpus, "memoryGiB": build.memory_gib, "diskGiB": build.disk_gib,
        "host": info.host.is_some(), "hostPort": info.host.as_ref().map_or(47800, |h| h.port),
        "installDir": info.install_dir,
    })
}

fn shared_paths(shares: &[Share]) -> Vec<PathBuf> {
    shares
        .iter()
        .filter(|s| s.location.is_absolute() && s.location == s.mount_point)
        .map(|s| s.location.clone())
        .collect()
}

fn in_share(path: &Path, shares: &[PathBuf]) -> bool {
    shares.iter().any(|share| path.starts_with(share))
}

/// A `[vm] modules` file as the build imports it, by its path on the Mac.
#[derive(Debug, PartialEq, Eq)]
enum Module {
    Shared(PathBuf),
    FirstBoot(PathBuf),
    Copied(PathBuf, Vec<u8>),
    Pinned(PathBuf, Vec<u8>),
}

impl Module {
    /// Its name in the system's build record: the same path `super::built::configured` names it by.
    fn name(&self) -> String {
        match self {
            Module::Shared(path) | Module::FirstBoot(path) | Module::Copied(path, _) | Module::Pinned(path, _) => {
                path.display().to_string()
            }
        }
    }
}

fn resolve_modules(
    config: &atomics_core::config::VmConfig,
    shares: &[PathBuf],
    writable: &[PathBuf],
    first_boot_peers: Option<&[PathBuf]>,
    warn: &dyn Fn(String),
) -> Result<Vec<Module>> {
    config.modules
        .iter()
        .map(|name| {
            let requested = atomics_core::paths::expand(name);
            let path = requested.canonicalize().with_context(|| format!("finding VM module {}", requested.display()))?;
            if !path.is_file() {
                bail!("VM module {} is not a file", path.display());
            }
            // A writer of the configured entry's folder can replace a symlink to a read-only file.
            let can_write = |folders: &[PathBuf]| {
                requested.parent().is_some_and(|dir| writable_folder_of(dir, folders).is_some())
                    || writable_folder_of(&path, folders).is_some()
            };
            if can_write(writable) {
                let pin = config.bootstrap_sha256.get(name);
                if pin.is_none() && in_share(&path, shares) && first_boot_peers.is_some_and(|peers| !can_write(peers)) {
                    return Ok(Module::FirstBoot(path));
                }
                let expected = pin.with_context(|| format!(
                    "[vm] modules: {name} is in a folder the VM can write. Use an independently verified [vm.bootstrap_sha256] entry for a standalone module, or move it to a read-only share"
                ))?;
                let contents = std::fs::read(&path)?;
                if !super::built::sha256(&contents).eq_ignore_ascii_case(expected) {
                    bail!("the module {name} differs from its verified bootstrap SHA-256");
                }
                warn(format!("using the verified copy of {name} as a standalone file; relative imports are unavailable"));
                return Ok(Module::Pinned(path, contents));
            }
            if in_share(&path, shares) {
                Ok(Module::Shared(path))
            } else {
                warn(format!(
                    "{} is outside the VM's shares; copying only this file. Relative paths in it will not resolve.",
                    path.display()
                ));
                let contents = std::fs::read(&path)?;
                Ok(Module::Copied(path, contents))
            }
        })
        .collect()
}

/// A shared checkout's exact HEAD, including worktrees. Never fetch, merge, or include dirty files.
fn source(configured: &str, shares: &[PathBuf], commit: Option<&str>) -> Result<String> {
    let checkout = atomics_core::paths::expand(configured);
    if !configured.trim().is_empty()
        && let Ok(checkout) = checkout.canonicalize()
        && in_share(&checkout, shares)
        && checkout.join("flake.nix").is_file()
        && let Ok(rev) = atomics_core::git::git(&checkout, &["rev-parse", "--verify", "HEAD^{commit}"])
        && git_dirs(&checkout)
            .is_ok_and(|dirs| dirs.iter().all(|dir| dir.canonicalize().is_ok_and(|path| in_share(&path, shares))))
    {
        // URL encoding also protects a path containing '?' or '#' from changing the flake ref.
        if let Ok(mut url) = url::Url::from_file_path(checkout) {
            url.set_query(Some(&format!("rev={}", rev.trim())));
            return Ok(format!("git+{url}"));
        }
    }
    let commit = commit
        .filter(|commit| commit.len() == 40 && commit.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .context("this ATOMICS build has no clean full commit; set [update] source to a local checkout shared with the VM (including its Git metadata)")?;
    Ok(format!("github:xmagit/ATOMICS/{commit}"))
}

pub(super) fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn encoded(value: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(value)
}

fn build_script(src: &str, settings: &Value, modules: &[Module]) -> String {
    let mut script = format!(
        "set -euo pipefail\ndir=$(mktemp -d /tmp/atomics-vm-update.XXXXXXXXXX)\ntrap 'rm -rf -- \"$dir\"' EXIT\nprintf %s {} | base64 -d > \"$dir/system.nix\"\n",
        quote(&encoded(EXPRESSION.as_bytes()))
    );
    let mut paths = Vec::new();
    for (index, module) in modules.iter().enumerate() {
        let path = match module {
            Module::Shared(path) | Module::FirstBoot(path) => path.display().to_string(),
            Module::Copied(_, contents) | Module::Pinned(_, contents) => {
                let name = format!("module-{index}.nix");
                script.push_str(&format!("printf %s {} | base64 -d > \"$dir/{name}\"\n", quote(&encoded(contents))));
                name
            }
        };
        paths.push(json!({"name": module.name(), "path": path}));
    }
    script.push_str(&format!(
        "args=(--extra-experimental-features 'nix-command flakes' build --impure --print-out-paths \\\n         --max-jobs auto -f \"$dir/system.nix\" --argstr src {} --argstr settings {} --argstr modules {})\n\
         top=$(nix \"${{args[@]}}\" --out-link \"$dir/system\" toplevel)\n\
         bundle=$(nix \"${{args[@]}}\" --out-link \"$dir/bundle\" bundle)\n\
         printf '%s\\n' \"$dir\" \"$top\" \"$bundle\"\ntrap - EXIT\n",
        quote(src), quote(&settings.to_string()), quote(&json!(paths).to_string())
    ));
    script
}

pub(super) fn guest(info: &BundleInfo, script: &str) -> Result<String> {
    let output = super::guest_shell(info)?
        // A login shell sets PATH and the profile up, then hands over to a plain bash: an `exit` in a login
        // shell would source /etc/bash_logout, which fails under the script's `set -u`.
        .args(["bash", "--login", "-c", "exec bash -c \"$1\"", "atomics-vm-update", script])
        .stderr(Stdio::inherit())
        .output()
        .context("running the VM update step")?;
    if !output.status.success() {
        bail!("the VM update step failed (above)");
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

/// Runs `script` where the system may be changed: as the admin user in hardened mode (super::system_shell).
fn system(info: &BundleInfo, script: &str) -> Result<String> {
    use std::io::Write;
    let mut child = super::system_shell(info)?
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("running the VM update step")?;
    child.stdin.take().context("the VM update step's stdin")?.write_all(script.as_bytes())?;
    let output = child.wait_with_output().context("running the VM update step")?;
    if !output.status.success() {
        bail!("the VM update step failed (above)");
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

pub(super) fn busy(answer: &Value) -> Result<Vec<String>> {
    if answer["ok"] != true || !answer["result"]["sessions"].is_array() {
        bail!("the VM's host did not answer list; keeping its current system");
    }
    // New hosts publish exactly what the in-app update waits for, including shells and editors.
    if let Some(blockers) = answer["result"].get("update_blockers") {
        return serde_json::from_value(blockers.clone()).context("reading the host's update blockers");
    }
    // 0.1.7 hosts predate that field: their list still reports turns, approvals and queue work.
    let mut blockers: Vec<String> = answer["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| {
            matches!(s["status"].as_str(), Some("working" | "starting" | "needs you"))
                || !s["approval"].is_null()
                || !s["queue"].is_null()
        })
        .map(|s| {
            format!(
                "{}: {}",
                s["status"].as_str().unwrap_or("busy"),
                s["title"].as_str().or(s["agent"].as_str()).unwrap_or("agent")
            )
        })
        .collect();
    for (field, description) in [("running", "queue slot held"), ("waiting", "queued work waiting")] {
        if let Some(tickets) = answer["result"]["resources"][field].as_array() {
            blockers.extend(
                tickets.iter().map(|ticket| format!("{description}: {}", ticket["label"].as_str().unwrap_or("work"))),
            );
        }
    }
    Ok(blockers)
}

fn refuse_busy(blockers: &[String], yes: bool) -> Result<()> {
    if !blockers.is_empty() {
        let work = blockers.join("\n  ");
        if !yes {
            bail!(
                "work in the VM is busy:\n  {work}\nstopped; wait for this work to finish, or use --yes to continue anyway."
            );
        }
    }
    Ok(())
}

#[derive(Default)]
struct BusyPolicy {
    yes: bool,
    announced: bool,
}

impl BusyPolicy {
    fn check(&mut self, blockers: &[String]) -> Result<Option<String>> {
        refuse_busy(blockers, self.yes)?;
        if blockers.is_empty() || self.announced {
            return Ok(None);
        }
        self.announced = true;
        Ok(Some(format!("Continuing with busy work (--yes):\n  {}", blockers.join("\n  "))))
    }
}

/// What keeps the VM from stopping now, as its host reports it.
pub(super) fn blockers(info: &BundleInfo) -> Result<Vec<String>> {
    busy(&host_work(info)?)
}

fn host_work(info: &BundleInfo) -> Result<Value> {
    // With the older agents-over-ssh layout this reaches the explicitly forwarded Mac host.
    // Without that forward there is no host to ask, so fail closed rather than interrupt agents.
    let answer = guest(info, r#"exec atomics ctl '{"cmd":"list"}'"#)?;
    serde_json::from_str(&answer).context("reading the VM host's list")
}

fn check_busy(info: &BundleInfo, policy: &mut BusyPolicy) -> Result<Value> {
    let work = host_work(info)?;
    if let Some(message) = policy.check(&busy(&work)?)? {
        println!("{message}");
    }
    Ok(work)
}

struct Built<'a> {
    info: &'a BundleInfo,
    dir: String,
    top: String,
    bundle: String,
}

impl Drop for Built<'_> {
    fn drop(&mut self) {
        if let Err(error) = system(self.info, &format!("rm -rf -- {}", quote(&self.dir))) {
            eprintln!("Could not remove the VM's temporary build roots: {error:#}");
        }
    }
}

fn store_path(path: &str) -> bool {
    path.strip_prefix("/nix/store/").is_some_and(|name| {
        name.len() > 33 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._+?=".contains(&b))
    })
}

/// Imports can cross device boundaries by relative or absolute path. Mask every writable
/// subtree wherever the build can see it, in a namespace private to this evaluation.
fn evaluation_shell(info: &BundleInfo, shares: &[Share], writable: &[PathBuf]) -> Result<Command> {
    let mut hidden = Vec::new();
    for share in shares {
        if writable_folder_of(&share.location, writable).is_some() {
            hidden.push(share.mount_point.clone());
        } else {
            // Another instance can export a subfolder of our read-only device under a different host path.
            // writable_folders includes the canonical spelling of each writable folder.
            let host = share.location.canonicalize().unwrap_or_else(|_| share.location.clone());
            for folder in writable {
                if let Some(relative) = relative_to(folder, &host) {
                    hidden.push(share.mount_point.join(relative));
                }
            }
        }
    }
    hidden.sort();
    hidden.dedup_by(|path, previous| path.starts_with(previous));
    if hidden.is_empty() {
        return super::system_shell(info);
    }
    let mask = r#"user=$1; shift
for path do mount -t tmpfs -o ro,nodev,nosuid,noexec tmpfs "$path"; done
exec sudo -H -u "$user" -- bash --login -c 'exec bash -s'"#;
    let paths: Vec<_> = hidden.iter().map(|path| quote(&path.to_string_lossy())).collect();
    super::system_command(
        info,
        &format!(
            "exec sudo unshare --mount --propagation private -- bash -eu -c {} atomics-vm-build \"$(id -un)\" {}",
            quote(mask),
            paths.join(" ")
        ),
    )
}

/// Stdin and stdout files let the existing queue runner carry a large script without putting it
/// in one ssh argument. Build roots stay in the guest until the bundle and boot selection are done.
fn build<'a>(info: &'a BundleInfo, script: &str, local: &Path, shell: Command) -> Result<Built<'a>> {
    let input = local.join("build.sh");
    let output = local.join("paths");
    std::fs::write(&input, script)?;
    // Built as the user that switches to it: an agent of the VM's user cannot hand the admin a system of its own.
    let mut command: Vec<String> = vec![
        "/bin/sh".into(),
        "-c".into(),
        r#"input=$1 output=$2; shift 2; exec "$@" < "$input" > "$output""#.into(),
        "atomics-vm-build".into(),
        input.display().to_string(),
        output.display().to_string(),
        shell.get_program().to_string_lossy().into_owned(),
    ];
    command.extend(shell.get_args().map(|arg| arg.to_string_lossy().into_owned()));
    // In client mode the host and its queue live in the guest. There is no local host to queue on.
    let code = if info.host.is_some() {
        Command::new(&command[0]).args(&command[1..]).status()?.code().unwrap_or(1)
    } else {
        crate::governor::run_queued(
            &atomics_core::paths::socket_path(),
            atomics_core::governor::SlotClass::Test,
            &command,
        )
    };
    if code != 0 {
        bail!("the VM build failed; nothing was switched and the VM still boots its current system");
    }
    let paths = std::fs::read_to_string(output)?;
    let lines: Vec<_> = paths.lines().collect();
    let [dir, top, bundle] = lines.as_slice() else { bail!("unexpected VM build output: {paths}") };
    if !dir
        .strip_prefix("/tmp/atomics-vm-update.")
        .is_some_and(|name| !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric()))
        || !store_path(top)
        || !store_path(bundle)
    {
        bail!("unexpected VM build paths: {paths}");
    }
    Ok(Built { info, dir: dir.to_string(), top: top.to_string(), bundle: bundle.to_string() })
}

/// Only the boot selection changes. On failure, restore both the system profile and bootloader.
fn switch_script(top: &str) -> String {
    format!(
        r#"set -euo pipefail
top={}
current=$(readlink -f /run/current-system)
profile=/nix/var/nix/profiles/system
previous=$(readlink -f "$profile")
if [ "$top" = "$previous" ]; then
  if [ "$top" = "$current" ]; then echo unchanged; else echo restart-pending; fi
  exit 0
fi
rollback() {{
  sudo nix-env -p "$profile" --set "$previous"
  sudo "$previous/bin/switch-to-configuration" boot >&2
}}
trap rollback ERR
sudo nix-env -p "$profile" --set "$top"
sudo "$top/bin/switch-to-configuration" boot >&2
trap - ERR
if [ "$top" = "$current" ]; then echo unchanged; else echo changed; fi
"#,
        quote(top)
    )
}

/// The guest's side of the bundle fetch: the built store path as one ustar stream on stdout. GNU tar's ustar
/// format has no long-name or pax entries, which `extract_bundle` would refuse anyway.
fn fetch_script(store: &str) -> String {
    format!("set -euo pipefail\nexec tar -C {} --format=ustar --numeric-owner -cf - .\n", quote(store))
}

/// Where the Mac bundle is read from: the same shell the system was built and is selected with
/// (super::system_shell). In hardened mode that is the admin user, over a private ssh connection with no shared
/// control master, so nothing the VM's own user runs, its ssh login included, can serve another tree here. It
/// is never `limactl copy` or `limactl shell`, which log in as that user. Without an admin the VM's user is the
/// one that builds and switches the system with sudo anyway: one trust domain (docs/nixos-vm.md).
fn bundle_source(info: &BundleInfo) -> Result<Command> {
    super::system_shell(info)
}

/// Streams the built bundle out of the guest through `source` (bundle_source) into `target`, which must not
/// exist yet.
fn fetch_bundle(mut source: Command, store: &str, target: &Path) -> Result<()> {
    use std::io::Write;
    let mut child = source
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("reading the new Mac bundle from the VM")?;
    child.stdin.take().context("the bundle fetch's stdin")?.write_all(fetch_script(store).as_bytes())?;
    let extracted = extract_bundle(child.stdout.take().context("the bundle fetch's stdout")?, target);
    let status = child.wait()?;
    extracted?;
    if !status.success() {
        bail!("reading the new Mac bundle from the VM failed");
    }
    Ok(())
}

const MAX_BUNDLE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_BUNDLE_ENTRIES: usize = 4096;

/// Unpacks a ustar stream of regular files and folders only: no links, devices, long-name or pax entries,
/// no absolute paths or `..`, no entry twice. Files get 0755 when executable for their owner, else 0644.
fn extract_bundle(mut reader: impl std::io::Read, target: &Path) -> Result<()> {
    use std::path::Component;
    fn field(header: &[u8], range: std::ops::Range<usize>) -> &[u8] {
        let bytes = &header[range];
        &bytes[..bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len())]
    }
    fn octal(header: &[u8], range: std::ops::Range<usize>) -> Result<u64> {
        let text = std::str::from_utf8(field(header, range))?.trim_matches(|c| c == ' ' || c == '\0');
        u64::from_str_radix(if text.is_empty() { "0" } else { text }, 8).context("a bad number in the bundle")
    }
    std::fs::create_dir(target)?;
    let (mut total, mut entries) = (0_u64, 0_usize);
    let mut header = [0_u8; 512];
    loop {
        reader.read_exact(&mut header).context("the bundle stream ended early")?;
        if header.iter().all(|&b| b == 0) {
            // tar pads its last record with zeros; read them, so it never writes into a closed pipe.
            std::io::copy(&mut reader.take(1024 * 1024), &mut std::io::sink())?;
            return Ok(());
        }
        entries += 1;
        if entries > MAX_BUNDLE_ENTRIES {
            bail!("the new Mac bundle has too many files");
        }
        let sum: u64 =
            header.iter().enumerate().map(|(i, &b)| if (148..156).contains(&i) { 32 } else { u64::from(b) }).sum();
        if &header[257..263] != b"ustar\0" || octal(&header, 148..156)? != sum {
            bail!("the new Mac bundle is not a plain ustar stream");
        }
        let (prefix, name) = (field(&header, 345..500), field(&header, 0..100));
        let name = if prefix.is_empty() {
            std::str::from_utf8(name)?.to_string()
        } else {
            format!("{}/{}", std::str::from_utf8(prefix)?, std::str::from_utf8(name)?)
        };
        let relative: PathBuf = Path::new(&name)
            .components()
            .filter_map(|c| match c {
                Component::CurDir => None,
                Component::Normal(part) => Some(Ok(part)),
                _ => Some(Err(anyhow::anyhow!("the new Mac bundle names {name:?}; nothing was installed"))),
            })
            .collect::<Result<_>>()?;
        let size = octal(&header, 124..136)?;
        let mode = octal(&header, 100..108)?;
        total += size;
        if total > MAX_BUNDLE_BYTES {
            bail!("the new Mac bundle is too large");
        }
        let path = target.join(&relative);
        match header[156] {
            b'5' if size == 0 => {
                if relative.as_os_str().is_empty() {
                    continue;
                }
                std::fs::create_dir(&path).with_context(|| format!("unpacking {name:?}"))?;
            }
            b'0' | 0 if !relative.as_os_str().is_empty() => {
                let mut contents = vec![0; size as usize];
                reader.read_exact(&mut contents).context("the bundle stream ended early")?;
                let padding = (512 - size % 512) % 512;
                reader.read_exact(&mut [0; 512][..padding as usize]).context("the bundle stream ended early")?;
                // A folder must have come first; create_new refuses a second entry of the same name.
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .with_context(|| format!("unpacking {name:?}"))?;
                std::io::Write::write_all(&mut file, &contents)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    file.set_permissions(std::fs::Permissions::from_mode(if mode & 0o100 != 0 {
                        0o755
                    } else {
                        0o644
                    }))?;
                }
                #[cfg(not(unix))]
                let _ = mode;
            }
            kind => bail!(
                "the new Mac bundle holds {name:?} of tar type {:?}, not a file or folder; nothing was installed",
                kind as char
            ),
        }
    }
}

fn validate_bundle(path: &Path) -> Result<()> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            validate_bundle(&entry.path())?;
        } else if !kind.is_file() || std::fs::read(entry.path())?.windows(10).any(|w| w == b"/nix/store") {
            bail!("the new Mac bundle contains a link or /nix/store reference; nothing was installed");
        }
    }
    Ok(())
}

fn install(bundle: &Path) -> Result<()> {
    if !Command::new("/bin/sh").arg(bundle.join("install.sh")).stdout(Stdio::null()).status()?.success() {
        bail!("the Mac bundle's install.sh failed; the VM's boot selection was not changed");
    }
    Ok(())
}

pub(super) fn load_config() -> Result<Config> {
    Config::load_existing(&atomics_core::paths::config_file())
}

pub fn update(options: Update) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!(super::MACOS_ONLY);
    }
    let _lima_lock = super::lifecycle_lock()?;
    let _lock = crate::update::lock(&atomics_core::paths::atomics_home())?;
    let config = load_config()?;
    let installed = super::installed_dir();
    let info = BundleInfo::read(&installed).context("no readable VM bundle; run atomics vm setup first")?;
    run(&installed, &info, &config, options)
}

pub(super) fn run(installed: &Path, info: &BundleInfo, config: &Config, options: Update) -> Result<()> {
    anyhow::ensure!(
        installed.canonicalize()? == Path::new(&info.install_dir).canonicalize()?,
        "the installed bundle's installDir differs from its location"
    );
    let state = super::lima_status_for_command(&info.instance)?
        .context("the Lima instance is missing; run atomics vm setup")?;
    let first_boot = options.new_vm && state == "Stopped";
    match state.as_str() {
        "Running" => {}
        "Stopped" if first_boot => {}
        "Stopped" => super::start_locked(&info.instance, installed)?,
        _ => bail!("VM {} is {state}; repair it before updating", info.instance),
    }
    let mut busy_policy = BusyPolicy { yes: options.yes, ..Default::default() };
    let lima = read_lima(&info.instance)?;
    let settings = settings(info, &build_info(info, &lima)?);
    let inventory = Inventory::read(&info.instance)?;
    let previous = Policy::for_start(&info.instance, installed, &inventory)?;
    let running = previous.running_info(installed, info)?;
    let info = &running;
    let devices = inventory.running.as_ref().unwrap_or(&inventory.configured);
    let peer_writable = writable_folders(&inventory.peers);
    let mut exposed = inventory.exports();
    exposed.extend(bundle_shares(installed).context("reading the installed bundle's shares")?);
    let writable = writable_folders(&exposed);
    if let Some(folder) = writable_overlap(installed, &writable) {
        bail!("installed bundle {} overlaps {}, a folder the VM can write", installed.display(), folder.display());
    }
    refuse_writable(&[], &config.update.source, &info.install_dir, &writable)?;
    let shares = if inventory.unknown_history { vec![] } else { shared_paths(devices) };
    let evaluation_writers = if inventory.unknown_history { vec![PathBuf::from("/")] } else { writable };

    let modules = resolve_modules(
        &config.vm,
        &shares,
        &evaluation_writers,
        first_boot.then_some(peer_writable.as_slice()),
        &|message| eprintln!("warning: {message}"),
    )?;
    let src = source(&config.update.source, &shares, crate::build_info::commit())?;
    let boot;
    let active_inventory = if first_boot {
        super::start_locked(&info.instance, installed)?;
        boot = Inventory::read(&info.instance)?;
        &boot
    } else {
        &inventory
    };
    let active = active_inventory.running.as_deref().context("cannot verify running devices")?;
    if first_boot {
        anyhow::ensure!(
            plan_mounts(&previous.shares, active).changes.is_empty() && active.iter().all(|s| !s.writable),
            "bootstrap requires read-only devices"
        );
    }
    let work = check_busy(info, &mut busy_policy)?;
    if info.host.is_some()
        && modules.iter().any(|module| matches!(module, Module::FirstBoot(_)))
        && work["result"]["sessions"].as_array().is_some_and(|sessions| !sessions.is_empty())
    {
        bail!(
            "the new VM has already hosted a session. Use [vm.bootstrap_sha256] for a standalone writable module or move the modules to a read-only share"
        );
    }
    println!("Source: {src}");
    println!(
        "Building the VM system and Mac bundle{}…",
        if info.host.is_some() { "" } else { " (waits for a test slot)" }
    );
    let local = tempfile::tempdir()?;
    let mut hidden = evaluation_writers;
    if first_boot {
        // The new disk's authorized module remains available for relative imports.
        // A peer's writable subfolder must still be hidden on that same device.
        hidden.retain(|folder| !modules.iter().any(|module| {
            matches!(module, Module::FirstBoot(path) if writable_folder_of(path, std::slice::from_ref(folder)).is_some())
        }));
        hidden.extend(writable_folders(&active_inventory.peers));
    }
    let shell = evaluation_shell(info, active, &hidden)?;
    let built = build(info, &build_script(&src, &settings, &modules), local.path(), shell)?;
    let bundle = local.path().join("bundle");
    fetch_bundle(bundle_source(info)?, &built.bundle, &bundle).context("nothing was switched")?;
    validate_bundle(&bundle)?;
    let new = BundleInfo::read(&bundle)?;
    if new.instance != info.instance
        || new.user != info.user
        || new.home != info.home
        || new.mac_user != info.mac_user
        || new.install_dir != info.install_dir
        || new.host != info.host
    {
        bail!("the modules changed the VM identity or connection settings; nothing was switched");
    }
    // The new system's shares: what Lima must mount after the restart, writable only where the system says.
    let system_shares = bundle_shares(&bundle).context("nothing was switched")?;
    refuse_writable(&config.vm.modules, &config.update.source, &info.install_dir, &writable_folders(&system_shares))
        .context("nothing was switched")?;
    active_inventory.check_exports(&system_shares)?;
    let mounts = active_inventory.mount_plan(&system_shares);
    check_busy(info, &mut busy_policy)?;
    let backup = local.path().join("previous");
    if !Command::new("/bin/cp").arg("-R").arg(installed).arg(&backup).status()?.success() {
        bail!("could not back up the installed Mac bundle; nothing was switched");
    }
    let mut policy = previous.clone();
    policy.shares = common_shares(&previous.shares, &system_shares);
    policy.save(&info.instance, active_inventory)?;
    if let Err(error) = install(&bundle) {
        install(&backup).context("restoring the previous Mac bundle")?;
        previous.save(&info.instance, active_inventory)?;
        return Err(error);
    }
    let selection = match system(info, &switch_script(&built.top)) {
        Ok(result) if matches!(result.as_str(), "changed" | "unchanged" | "restart-pending") => result,
        result => {
            install(&backup).context("restoring the previous Mac bundle")?;
            bail!(
                "could not select the VM's next boot: {result:?}; see the guest's boot staging and rollback errors above"
            );
        }
    };
    policy.shares.clone_from(&system_shares);
    let mut identity = Identity::new(installed, info)?;
    identity.selected_admin.clone_from(&new.admin);
    if identity.current_admin != identity.selected_admin {
        identity.staged_from_boot.clone_from(&active_inventory.boot_id);
    }
    policy.identity = Some(identity);
    policy.save(&info.instance, active_inventory)?;
    drop(built);
    // Lima changes an instance's mounts only while it is stopped, so a difference there owes a restart too.
    let restart_owed = selection != "unchanged" || mounts.set.is_some();
    println!("VM system: {}", if selection == "restart-pending" { "restart pending" } else { &selection });
    println!("Mac bundle: reinstalled");
    if !mounts.changes.is_empty() {
        println!("Lima mounts: pending until the VM is stopped:\n  {}", mounts.changes.join("\n  "));
    }
    if options.no_restart {
        println!(
            "VM: {}",
            if restart_owed {
                "restart pending (--no-restart); run atomics vm update again when ready"
            } else {
                "not restarted (--no-restart; system unchanged)"
            }
        );
    } else {
        // Work could have started while the bundle was installed and the next boot was selected.
        if restart_owed {
            // Share the notice policy with the post-quit check; both safety checks still run.
            check_busy(info, &mut busy_policy)
                .context("restart pending; run atomics vm update again when the VM is idle")?;
        }
        restart_window(info, restart_owed, installed, &mut busy_policy)?;
        println!("VM: {}", if restart_owed { "restarted" } else { "not restarted (system unchanged)" });
        if mounts.set.is_some() {
            println!("Lima mounts: as the system mounts them");
        }
    }
    // With --no-restart the running system still has the previous administrator account.
    super::secrets::push(if options.no_restart { info } else { &new })?;
    Ok(())
}

fn restart_window(info: &BundleInfo, changed: bool, installed: &Path, busy: &mut BusyPolicy) -> Result<()> {
    let app = crate::setup::executable_path()
        .ok()
        .and_then(|exe| crate::restart::bundle_of(&exe).map(Path::to_path_buf))
        .unwrap_or_else(|| atomics_core::paths::home().join("Applications/ATOMICS.app"));
    quit_then_reopen(
        || quit_windows(Command::new("osascript")),
        || {
            if changed {
                check_busy(info, busy).context("restart pending; run atomics vm update again when the VM is idle")?;
                restart_guest(|| super::stop(&info.instance), || super::start_locked(&info.instance, installed))?;
            }
            Ok(())
        },
        || {
            if app.is_dir() && !reopen(&app, std::env::vars_os()).status()?.success() {
                bail!("the update is installed, but the ATOMICS window could not reopen");
            }
            Ok(())
        },
    )
}

/// Lima's mounts change while the VM is stopped. If they cannot be made to match, the VM stays stopped rather
/// than boot a system that mounts a folder read-only on a device root could still write.
fn restart_guest(stop: impl FnOnce() -> Result<()>, start: impl FnOnce() -> Result<()>) -> Result<()> {
    stop().context("restart pending: could not stop the VM; run atomics vm update again when ready")?;
    start().context("the new boot is installed, but the VM is stopped; run atomics vm start")
}

/// Even a partial quit must reopen the app. Keep the primary failure if reopening also fails.
fn quit_then_reopen(
    quit: impl FnOnce() -> Result<()>,
    restart: impl FnOnce() -> Result<()>,
    reopen: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let result = quit().and_then(|()| restart());
    let opened = reopen();
    match (result, opened) {
        (Err(error), Err(open)) => Err(error.context(format!("also could not reopen the app: {open:#}"))),
        (Err(error), _) => Err(error),
        (Ok(()), opened) => opened,
    }
}

/// LaunchServices can run several instances of one bundle. Address every instance, rather than the
/// single application selected by AppleScript's `application id`. Termination stays graceful.
fn quit_windows(mut command: Command) -> Result<()> {
    let script = r#"ObjC.import('AppKit');
const apps = $.NSRunningApplication.runningApplicationsWithBundleIdentifier('ai.atomisk.atomics');
for (let i = 0; i < apps.count; i++) apps.objectAtIndex(i).terminate;
for (let attempt = 0; attempt < 300; attempt++) {
    let remaining = [];
    for (let i = 0; i < apps.count; i++) {
        const app = apps.objectAtIndex(i);
        if (!app.terminated) remaining.push(ObjC.unwrap(app.localizedName) + ' (pid ' + app.processIdentifier + ')');
    }
    if (remaining.length === 0) break;
    if (attempt === 299) throw new Error('Could not quit: ' + remaining.join(', '));
    $.NSRunLoop.currentRunLoop.runUntilDate($.NSDate.dateWithTimeIntervalSinceNow(0.1));
}"#;
    let output = command.args(["-l", "JavaScript", "-e", script]).output().context("asking ATOMICS windows to quit")?;
    if !output.status.success() {
        bail!(
            "restart pending: {}; close those windows, then run atomics vm update again",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// `open -a app` as the Dock would open it: with this command's environment, but none of its `ATOMICS_*`
/// variables. `open` hands its environment to the app it starts, and a shell that pointed this command at a
/// scratch config or home (`ATOMICS_CONFIG`, `ATOMICS_HOME`, `ATOMICS_SOCKET`, …) would otherwise bring the
/// window up on those, as a host of its own, instead of as the VM's client. The app reads its own config.
fn reopen(app: &Path, env: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>) -> Command {
    let mut command = Command::new("open");
    command
        .arg("-a")
        .arg(app)
        .env_clear()
        .envs(env.into_iter().filter(|(key, _)| !key.to_string_lossy().starts_with("ATOMICS_")));
    command
}

#[cfg(test)]
mod tests;
