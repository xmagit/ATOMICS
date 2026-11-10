//! Build the guest's next boot and its Mac bundle without replacing the Lima disk.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use atomics_core::config::Config;
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{BuildInfo, BundleInfo};

pub struct Update {
    pub yes: bool,
    pub no_restart: bool,
}

/// Uses mac-build.nix's settings, retaining the installed bundle's identity too. Extra modules are
/// imported at their original location when shared.
const EXPRESSION: &str = r#"{ src, settings, modules }:
let
  s = builtins.fromJSON settings;
  vm = (builtins.getFlake src).nixosConfigurations.atomics-vm.extendModules {
    modules = [ {
      atomics.vm = {
        inherit (s) macUser cpus memoryGiB diskGiB user;
        host.enable = s.host;
        host.port = s.hostPort;
      };
      programs.atomics.macDesktop = {
        installDir = s.installDir;
        lima.instance = s.instance;
      };
    } ] ++ map (path: if builtins.substring 0 1 path == "/"
      then builtins.toPath path else ./. + ("/" + path)) (builtins.fromJSON modules);
  };
in {
  toplevel = vm.config.system.build.toplevel;
  bundle = vm.config.programs.atomics.macDesktop.bundle;
}
"#;

#[derive(Debug, Deserialize)]
pub(super) struct LimaConfig {
    cpus: u32,
    memory: String,
    disk: String,
    #[serde(default)]
    mounts: Vec<Mount>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Mount {
    location: String,
    mount_point: Option<String>,
}

impl LimaConfig {
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

fn shared_paths(lima: &LimaConfig) -> Vec<PathBuf> {
    lima.mounts
        .iter()
        .filter_map(|mount| {
            let path = atomics_core::paths::expand(&mount.location);
            let guest = mount.mount_point.as_ref().map(|p| atomics_core::paths::expand(p));
            (path.is_absolute() && guest.as_ref().is_none_or(|guest| guest == &path)).then_some(path)
        })
        .collect()
}

fn in_share(path: &Path, shares: &[PathBuf]) -> bool {
    shares.iter().any(|share| path.starts_with(share))
}

#[derive(Debug, PartialEq, Eq)]
enum Module {
    Shared(PathBuf),
    Copied(Vec<u8>),
}

fn resolve_modules(paths: &[String], shares: &[PathBuf], warn: &dyn Fn(String)) -> Result<Vec<Module>> {
    paths
        .iter()
        .map(|path| {
            let path = atomics_core::paths::expand(path);
            let path = path.canonicalize().with_context(|| format!("finding VM module {}", path.display()))?;
            if !path.is_file() {
                bail!("VM module {} is not a file", path.display());
            }
            if in_share(&path, shares) {
                Ok(Module::Shared(path))
            } else {
                warn(format!(
                    "{} is outside the VM's shares; copying only this file. Relative paths in it will not resolve.",
                    path.display()
                ));
                Ok(Module::Copied(std::fs::read(path)?))
            }
        })
        .collect()
}

/// A shared checkout's exact HEAD, including worktrees. Never fetch, merge, or include dirty files.
fn source(configured: &str, shares: &[PathBuf], version: &str) -> String {
    let checkout = atomics_core::paths::expand(configured);
    if !configured.trim().is_empty()
        && let Ok(checkout) = checkout.canonicalize()
        && in_share(&checkout, shares)
        && checkout.join("flake.nix").is_file()
        && let Ok(rev) = atomics_core::git::git(&checkout, &["rev-parse", "--verify", "HEAD^{commit}"])
        && ["--git-dir", "--git-common-dir"].iter().all(|flag| {
            atomics_core::git::git(&checkout, &["rev-parse", "--path-format=absolute", flag])
                .ok()
                .and_then(|path| Path::new(path.trim()).canonicalize().ok())
                .is_some_and(|path| in_share(&path, shares))
        })
    {
        // URL encoding also protects a path containing '?' or '#' from changing the flake ref.
        if let Ok(mut url) = url::Url::from_file_path(checkout) {
            url.set_query(Some(&format!("rev={}", rev.trim())));
            return format!("git+{url}");
        }
    }
    format!("github:xmagit/ATOMICS/v{version}")
}

fn quote(value: &str) -> String {
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
        match module {
            Module::Shared(path) => paths.push(path.display().to_string()),
            Module::Copied(contents) => {
                let name = format!("module-{index}.nix");
                script.push_str(&format!("printf %s {} | base64 -d > \"$dir/{name}\"\n", quote(&encoded(contents))));
                paths.push(name);
            }
        }
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

fn guest(info: &BundleInfo, script: &str) -> Result<String> {
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

fn busy(answer: &Value) -> Result<Vec<String>> {
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
            bail!("work in the VM is busy:\n  {work}\nstopped; nothing changed. Use --yes to continue anyway.");
        }
        println!("Continuing with busy work (--yes):\n  {work}");
    }
    Ok(())
}

fn check_busy(info: &BundleInfo, yes: bool) -> Result<()> {
    // With the older agents-over-ssh layout this reaches the explicitly forwarded Mac host.
    // Without that forward there is no host to ask, so fail closed rather than interrupt agents.
    let answer = guest(info, r#"exec atomics ctl '{"cmd":"list"}'"#)?;
    refuse_busy(&busy(&serde_json::from_str(&answer).context("reading the VM host's list")?)?, yes)
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

/// Stdin and stdout files let the existing queue runner carry a large script without putting it
/// in one ssh argument. Build roots stay in the guest until the bundle and boot selection are done.
fn build<'a>(info: &'a BundleInfo, script: &str, local: &Path) -> Result<Built<'a>> {
    let input = local.join("build.sh");
    let output = local.join("paths");
    std::fs::write(&input, script)?;
    // Built as the user that switches to it: an agent of the VM's user cannot hand the admin a system of its own.
    let shell = super::system_shell(info)?;
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
    let code = crate::governor::run_queued(
        &atomics_core::paths::socket_path(),
        atomics_core::governor::SlotClass::Test,
        &command,
    );
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
if [ "$top" = "$current" ] && [ "$top" = "$previous" ]; then echo unchanged; exit 0; fi
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
    let config = load_config()?;
    let installed = super::installed_dir();
    let info = BundleInfo::read(&installed).context("no readable VM bundle; run atomics vm setup first")?;
    run(&installed, &info, &config, options)
}

pub(super) fn run(installed: &Path, info: &BundleInfo, config: &Config, options: Update) -> Result<()> {
    let _lock = crate::update::lock(&atomics_core::paths::atomics_home())?;
    let state = super::lima_status(&info.instance)?.context("the Lima instance is missing; run atomics vm setup")?;
    match state.as_str() {
        "Running" => {}
        "Stopped" => super::start(&info.instance)?,
        _ => bail!("VM {} is {state}; repair it before updating", info.instance),
    }
    check_busy(info, options.yes)?;
    let lima = read_lima(&info.instance)?;
    let settings = settings(info, &build_info(info, &lima)?);
    let shares = shared_paths(&lima);
    let modules = resolve_modules(&config.vm.modules, &shares, &|message| eprintln!("warning: {message}"))?;
    let src = source(&config.update.source, &shares, env!("CARGO_PKG_VERSION"));
    println!("Source: {src}");
    println!("Building the VM system and Mac bundle (waits for a test slot)…");
    let local = tempfile::tempdir()?;
    let built = build(info, &build_script(&src, &settings, &modules), local.path())?;
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
    if Path::new(&info.install_dir).canonicalize()? != installed.canonicalize()? {
        bail!("the installed bundle's installDir differs from its location; nothing was switched");
    }
    check_busy(info, options.yes)?;
    let backup = local.path().join("previous");
    if !Command::new("/bin/cp").arg("-R").arg(installed).arg(&backup).status()?.success() {
        bail!("could not back up the installed Mac bundle; nothing was switched");
    }
    if let Err(error) = install(&bundle) {
        install(&backup).context("restoring the previous Mac bundle")?;
        return Err(error);
    }
    let changed = match system(info, &switch_script(&built.top)) {
        Ok(result) if result == "changed" || result == "unchanged" => result == "changed",
        result => {
            install(&backup).context("restoring the previous Mac bundle")?;
            bail!(
                "could not select the VM's next boot: {result:?}; see the guest's boot staging and rollback errors above"
            );
        }
    };
    println!("VM system: {}", if changed { "changed" } else { "unchanged" });
    println!("Mac bundle: reinstalled");
    if options.no_restart {
        println!("VM: not restarted (--no-restart); the selected system takes effect at the next boot");
    } else {
        restart_window(info, changed)?;
        println!("VM: {}", if changed { "restarted" } else { "not restarted (system unchanged)" });
    }
    Ok(())
}

fn restart_window(info: &BundleInfo, changed: bool) -> Result<()> {
    let app = crate::setup::executable_path()
        .ok()
        .and_then(|exe| crate::restart::bundle_of(&exe).map(Path::to_path_buf))
        .unwrap_or_else(|| atomics_core::paths::home().join("Applications/ATOMICS.app"));
    if app.is_dir() {
        let script = r#"if application id "ai.atomisk.atomics" is running then
  tell application id "ai.atomisk.atomics" to quit
  repeat 300 times
    if application id "ai.atomisk.atomics" is not running then return
    delay 0.1
  end repeat
  error "The ATOMICS window did not quit; the VM was not restarted."
end if"#;
        if !Command::new("osascript").args(["-e", script]).status()?.success() {
            bail!("the next boot is installed, but the window could not quit; restart the VM when ready");
        }
    }
    let restart =
        if changed { super::stop(&info.instance).and_then(|()| super::start(&info.instance)) } else { Ok(()) };
    // Reopen even if Lima could not restart, so the window can show the VM's state.
    if app.is_dir() && !reopen(&app, std::env::vars_os()).status()?.success() {
        bail!("the update is installed, but the ATOMICS window could not reopen");
    }
    restart.context("the next boot is installed, but Lima could not restart; run atomics vm start")
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
