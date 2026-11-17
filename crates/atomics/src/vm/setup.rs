//! `atomics vm setup|start|stop|status|check|config`: from a built image and bundle to a checked,
//! running VM. Each step says what it does; the steps that cannot be undone (replacing an existing
//! instance, writing the host config file) need the user's explicit word.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use atomics_core::config::Config;
use atomics_core::selfconfig::{self, Layer, Proposal, Scope, Source};

use super::{BundleInfo, Status};

/// The guest check, run in the guest from stdin, so it needs no checkout there.
const CHECK_SCRIPT: &str = include_str!("../../../../scripts/check-atomics-vm.sh");

pub struct Setup {
    /// The build's output folder: `atomics-vm.raw` and `bundle/`.
    pub from: Option<PathBuf>,
    pub sizes: super::download::Sizes,
    /// Replace an instance of the bundle's name that this setup did not create.
    pub replace: bool,
    /// Answer yes to the confirmation `replace` asks for (non-interactive use).
    pub yes: bool,
}

fn say(step: &str) {
    println!("\n── {step}");
}

/// The image and bundle a build left in `dir`.
fn build_outputs(dir: &Path) -> Result<(PathBuf, PathBuf)> {
    let image = dir.join("atomics-vm.raw");
    let bundle = dir.join("bundle");
    if !image.is_file() || !bundle.join("install.sh").is_file() {
        bail!(
            "{} has no atomics-vm.raw and bundle/; build them with scripts/macos-vm-build.sh (it prints the folder)",
            dir.display()
        );
    }
    Ok((std::fs::canonicalize(image)?, bundle))
}

/// Lima 2.2 or newer, and Rosetta for the guest's amd64 containers.
fn check_mac() -> Result<()> {
    let limactl =
        super::limactl().context("limactl is not installed; install Lima 2.2 or newer (brew install lima)")?;
    let output = std::process::Command::new(&limactl).arg("--version").output().context("running limactl --version")?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text.split_whitespace().last().unwrap_or("");
    let parts: Vec<u32> = version.split('.').take(2).filter_map(|p| p.parse().ok()).collect();
    if parts.len() < 2 || (parts[0], parts[1]) < (2, 2) {
        bail!("Lima {version} is too old; the VM needs 2.2 or newer");
    }
    println!("Lima {version} at {}", limactl.display());
    if !Path::new("/Library/Apple/usr/libexec/oah/libRosettaRuntime").exists() {
        bail!("Rosetta is not installed; run `softwareupdate --install-rosetta --agree-to-license`");
    }
    println!("Rosetta is installed");
    Ok(())
}

fn confirm(question: &str, expected: &str) -> bool {
    print!("{question} ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok() && line.trim() == expected
}

pub fn setup(options: Setup) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!(super::MACOS_ONLY);
    }
    say("This Mac");
    check_mac()?;

    say("The bundle");
    let downloaded;
    let from = match &options.from {
        Some(from) => from.clone(),
        None => {
            downloaded = super::download::prepare(&options.sizes, &|message| println!("{message}"))?;
            downloaded.path().to_path_buf()
        }
    };
    let (image, bundle) = build_outputs(&from)?;
    let info = BundleInfo::read(&bundle)?;
    let installed = PathBuf::from(&info.install_dir);

    say(&format!("The Lima instance {}", info.instance));
    let _lock = crate::update::lock(&atomics_core::paths::atomics_home())?;
    let _lima_lock = super::lifecycle_lock()?;
    let shares = super::update::bundle_shares(&bundle)?;
    let state = super::lima_status_for_command(&info.instance)?;
    let mut exposed = super::update::other_shares(&info.instance)?;
    let devices = match &state {
        Some(_) => super::update::read_lima(&info.instance)?.shares(),
        None => Vec::new(),
    };
    exposed.extend_from_slice(&devices);
    // The installer runs on this Mac. Its source and destination must be outside existing writable devices.
    let writable = super::update::writable_folders(&exposed);
    for path in [&bundle, &bundle.join("install.sh"), &image] {
        if let Some(folder) = super::update::writable_folder_of(path, &writable) {
            bail!("setup input {} is inside writable share {}", path.display(), folder.display());
        }
    }
    exposed.extend_from_slice(&shares);
    super::update::refuse_writable(&[], "", &info.install_dir, &super::update::writable_folders(&exposed))?;
    let retained = state.is_some() && super::created_here(&info.instance);
    match state {
        Some(state) if retained => {
            let previous = BundleInfo::read(&installed).context("cannot verify the retained instance's bundle")?;
            let policy = super::update::mount_policy(&info.instance, &installed)?;
            if previous.instance != info.instance
                || previous.admin != info.admin
                || !super::update::plan_mounts(&shares, &policy).changes.is_empty()
                || super::update::common_shares(&shares, &devices) != shares
            {
                bail!(
                    "keeping the retained VM's bundle and devices. To change its system and shares together, configure the intended modules and run atomics vm update"
                );
            }
            println!("{} exists ({}); it was set up here before, so it is kept", info.instance, state.to_lowercase());
        }
        Some(state) => {
            println!(
                "An instance named {} already exists ({}), and this setup did not create it.",
                info.instance,
                state.to_lowercase()
            );
            if !options.replace {
                bail!(
                    "keeping it. Build the bundle for another instance name (scripts/macos-vm-build.sh --instance NAME), \
                     or run again with --replace to delete {} and its disk",
                    info.instance
                );
            }
            let sure = options.yes
                || confirm(
                    &format!("Delete {0} and everything on its disk? Type {0} to confirm:", info.instance),
                    &info.instance,
                );
            if !sure {
                bail!("not confirmed; {} is kept", info.instance);
            }
            let _ = super::stop(&info.instance);
            let status = super::limactl_command()?.args(["delete", "--force", &info.instance]).status()?;
            if !status.success() {
                bail!("limactl delete {} failed", info.instance);
            }
            super::forget(&info.instance);
        }
        None => {}
    }

    let status = std::process::Command::new("/bin/sh")
        .arg(bundle.join("install.sh"))
        .stdout(Stdio::null())
        .status()
        .context("running the bundle's install.sh")?;
    if !status.success() {
        bail!("the bundle's install.sh failed");
    }
    println!("Installed in {} (instance {}, VM user {})", info.install_dir, info.instance, info.user);
    if !retained {
        create(&info, &installed, &image)?;
    }
    super::update::save_mount_policy(&info.instance, &shares)?;

    say("Lima mounts");
    match_mounts(&info, &shares)?;

    say("Start");
    match super::status(&info.instance, false) {
        Status::Running => println!("{} is running", info.instance),
        _ => {
            println!("Starting {} (waits for a QA slot)", info.instance);
            super::start_locked(&info.instance, &installed)?;
        }
    }

    say("Check the guest");
    check(&info)?;

    let config = super::update::load_config()?;
    modules_step(
        &mut std::io::stdout(),
        !retained,
        &installed,
        &config.vm.modules,
        |info| {
            super::update::run(&installed, info, &config, new_vm_update())?;
            // The updated bundle's: hardened mode inspects sshd as its admin user.
            check(&BundleInfo::read(&installed)?)
        },
        super::built::probe,
    )?;

    say("Sync keychain secrets to the VM broker");
    // Applying modules may have installed hardened mode and a new administrator account.
    // Use that bundle's connection metadata, never the initial image's user/sudo path.
    super::secrets::push(&BundleInfo::read(&installed)?)?;

    say("Sign-ins left");
    sign_ins(&info);

    say("This Mac's ATOMICS config");
    match propose_config(&installed)? {
        Some(proposal) => show(&proposal),
        None => println!("Nothing to change."),
    }
    Ok(())
}

/// Applies `[vm] modules` to an instance this setup just created, with `apply`; then, whether or not it did, says
/// whether the running system is what this Mac configures, and how to make it so. A VM set up before, or modules
/// configured since, are not applied here: that restarts the VM, which `atomics vm update` does when asked.
fn modules_step(
    out: &mut dyn Write,
    created: bool,
    installed: &Path,
    modules: &[String],
    apply: impl FnOnce(&BundleInfo) -> Result<()>,
    probe: impl FnOnce(&BundleInfo) -> Result<super::built::Guest>,
) -> Result<()> {
    if created && !modules.is_empty() {
        writeln!(out, "\n── Apply your NixOS modules")?;
        apply(&BundleInfo::read(installed)?)?;
    }
    writeln!(out, "\n── Your NixOS modules and hardened mode")?;
    // Applying modules may have installed hardened mode, and with it an admin user to ask.
    let info = BundleInfo::read(installed)?;
    super::built::say(out, &super::built::report_with(&info, modules, probe))?;
    Ok(())
}

/// How setup applies `[vm] modules` to the instance it just created: asking before busy work and restarting it as an
/// update does, with a verified copy for a module in a writable share.
fn new_vm_update() -> super::update::Update {
    super::update::Update { yes: false, no_restart: false, new_vm: true }
}

fn create(info: &BundleInfo, installed: &Path, image: &Path) -> Result<()> {
    let location = format!(".images[0].location = {:?}", image.display().to_string());
    println!("Creating {} from {}", info.instance, image.display());
    let status = super::limactl_command()?
        .args(["create", &format!("--name={}", info.instance), "--tty=false", "--set", &location])
        .arg(installed.join("lima.yaml"))
        .status()
        .context("running limactl create")?;
    if !status.success() {
        bail!("limactl create {} failed", info.instance);
    }
    super::remember(&info.instance, image)
}

/// The instance mounts exactly the shares of the system the installed bundle was built with, each as writable
/// as that system mounts it (docs/nixos-vm.md, Hardened mode). Lima changes mounts only while the instance is
/// stopped, so a running one is stopped first, but never under busy work.
fn match_mounts(info: &BundleInfo, built: &[super::update::Share]) -> Result<()> {
    let plan = super::update::plan_mounts(built, &super::update::read_lima(&info.instance)?.shares());
    if plan.changes.is_empty() {
        println!("{} shares the system's folders as the system mounts them", info.instance);
        return Ok(());
    }
    println!("{} differs from the system's shares:\n  {}", info.instance, plan.changes.join("\n  "));
    if super::status(&info.instance, false) == Status::Running {
        let stop_later = "Lima changes mounts only while the VM is stopped";
        let blockers = super::update::blockers(info).with_context(|| {
            format!("{stop_later}, and its host could not say whether work is running; stop it when idle (atomics vm stop) and run atomics vm setup again")
        })?;
        if !blockers.is_empty() {
            bail!(
                "{stop_later}, and work in the VM is busy:\n  {}\nrun atomics vm setup again when it is done",
                blockers.join("\n  ")
            );
        }
        println!("Stopping {} to change its mounts", info.instance);
        super::stop(&info.instance)?;
    }
    super::update::apply_mounts(&info.instance, built)?;
    println!("{} now shares the system's folders as the system mounts them", info.instance);
    Ok(())
}

/// scripts/check-atomics-vm.sh in the guest, through the QA queue.
pub fn check(info: &BundleInfo) -> Result<()> {
    let limactl = super::limactl().context("limactl is not installed")?;
    let local = tempfile::tempdir()?;
    std::fs::write(local.path().join("check.sh"), CHECK_SCRIPT)?;
    std::fs::write(local.path().join("sshd.sh"), "set -eu\nsudo sshd -T\n")?;
    // The guest compares Lima's mounts with the shares its system mounts.
    let mounts = super::update::read_lima(&info.instance)?.shares();
    std::fs::write(local.path().join("lima-mounts"), serde_json::to_string(&mounts)?)?;
    let runner = check_runner(info, &limactl, local.path())?;
    let mut command = vec![runner.get_program().to_string_lossy().into_owned()];
    command.extend(runner.get_args().map(|arg| arg.to_string_lossy().into_owned()));
    // A VM client has no local queue. Its host is in the guest.
    let passed = if info.host.is_some() {
        Command::new(&command[0]).args(&command[1..]).status()?.success()
    } else {
        super::queued(&command)
    };
    if !passed {
        bail!("the guest check failed (above)");
    }
    Ok(())
}

/// Inspect sshd through the same admin path as updates, then pass only its output to the ordinary
/// user's checks. The rest must run as that user to check its host, rootless Docker and permissions.
/// Lima's mounts, read on this Mac, go along as data (`lima-mounts` in `local`).
fn check_runner(info: &BundleInfo, limactl: &Path, local: &Path) -> Result<Command> {
    #[cfg(test)]
    crate::test_support::assert_fixture_program("limactl", limactl);
    let system = super::system_shell(info)?;
    let mut runner = Command::new("/bin/sh");
    runner.args([
        "-c",
        r#"dir=$1 lima=$2 home=$3 instance=$4 hardened=$5; shift 5
"$@" < "$dir/sshd.sh" > "$dir/sshd-config" || exit $?
exec "$lima" shell --workdir="$home" "$instance" bash --login -c 'exec bash -s -- "$@"' atomics-vm-check --sshd-config "$(cat "$dir/sshd-config")" --hardened "$hardened" --lima-mounts "$(cat "$dir/lima-mounts")" < "$dir/check.sh""#,
        "atomics-vm-check",
    ]);
    runner.arg(local).arg(limactl).args([
        info.home.as_str(),
        info.instance.as_str(),
        if info.admin.is_some() { "true" } else { "false" },
    ]);
    runner.arg(system.get_program()).args(system.get_args());
    #[cfg(test)]
    {
        runner.env("HOME", local.join("home"));
        crate::test_support::isolate_environment(&mut runner);
    }
    Ok(runner)
}

/// `atomics-onboard --check` in the guest, and how to sign in to what is missing. Nothing is signed in here.
pub fn sign_ins(info: &BundleInfo) {
    let output = super::guest_shell(info).and_then(|mut command| {
        command.args(["atomics-onboard", "--check"]).output().context("running atomics-onboard --check")
    });
    match output {
        Ok(output) => {
            print!("{}", String::from_utf8_lossy(&output.stdout));
            if output.status.success() {
                println!("Every sign-in is done.");
            } else {
                println!(
                    "Sign in to what is missing in the VM; each step shows a code to enter in this Mac's browser:\n  \
                     limactl shell --workdir={} {} atomics-onboard",
                    info.home, info.instance
                );
            }
        }
        Err(error) => println!("Could not ask the guest: {error:#}"),
    }
}

/// The change the bundle's `config.d.toml` makes to this Mac's host config file, or `None` when it is
/// there already.
pub fn propose_config(installed: &Path) -> Result<Option<Proposal>> {
    let fragment = std::fs::read_to_string(installed.join("config.d.toml"))
        .with_context(|| format!("reading {}", installed.join("config.d.toml").display()))?;
    let [main, host] = Config::source_paths(&atomics_core::paths::config_file());
    let layers = [Layer::read(Source::Main, &main), Layer::read(Source::Host, &host)];
    match selfconfig::propose(Scope::Host, &layers, &fragment, &[]) {
        Ok(proposal) => Ok(Some(proposal)),
        Err(why) if why.starts_with("the change sets nothing new") => Ok(None),
        Err(why) => bail!("the bundle's config.d.toml does not apply to {}: {why}", host.display()),
    }
}

fn show(proposal: &Proposal) {
    println!("The bundle's settings for this Mac, as a change to {}:\n", proposal.file);
    print!("{}", proposal.diff);
    for warning in &proposal.warnings {
        println!("note: {warning}");
    }
    println!("\nNothing was written. Review it, then apply it with: atomics vm config --apply");
}

/// `atomics vm config [--apply]`.
pub fn config(apply: bool) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!(super::MACOS_ONLY);
    }
    let Some((installed, info)) = BundleInfo::installed() else {
        bail!("no VM bundle is installed; run atomics vm setup first")
    };
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    config_with(&mut std::io::stdout(), propose_config(&installed)?, apply, stamp, || {
        let config = super::update::load_config()?;
        Ok(super::built::report_now(&info, &config.vm.modules))
    })
}

/// `atomics vm config`, given the bundle's proposal. Applying it ends with what the VM runs against the config as it
/// is now, `report`, and the fix when they differ.
fn config_with(
    out: &mut dyn Write,
    proposal: Option<Proposal>,
    apply: bool,
    stamp: u64,
    report: impl FnOnce() -> Result<super::built::Report>,
) -> Result<()> {
    match proposal {
        None => writeln!(out, "The host config file already has the bundle's settings.")?,
        Some(proposal) if !apply => {
            show(&proposal);
            return Ok(());
        }
        Some(proposal) => {
            write!(out, "{}", proposal.diff)?;
            writeln!(out, "{}", apply_config(&proposal, stamp)?)?;
        }
    }
    if apply {
        writeln!(out)?;
        super::built::say(out, &report()?)?;
    }
    Ok(())
}

fn apply_config(proposal: &Proposal, stamp: u64) -> Result<String> {
    let file = atomics_core::paths::expand(&proposal.file);
    let backup = selfconfig::write(&file, proposal.before.as_deref(), &proposal.after, stamp)?;
    Ok(if proposal.before.as_deref() == Some(proposal.after.as_str()) {
        format!("No changes to {}.", file.display())
    } else if let Some(backup) = backup {
        format!("Wrote {} (the old file is {})", file.display(), backup.display())
    } else {
        format!("Wrote {}", file.display())
    })
}

/// The installed bundle, for the commands after setup.
fn installed() -> Result<BundleInfo> {
    if !cfg!(target_os = "macos") {
        bail!(super::MACOS_ONLY);
    }
    BundleInfo::installed().map(|(_, info)| info).context("no VM bundle is installed; run atomics vm setup")
}

pub fn status() -> Result<()> {
    let info = installed()?;
    let status = super::status_for_command(&info.instance);
    println!("{}: {}", info.instance, status.label());
    if let Some(host) = &info.host {
        println!("ATOMICS host in the VM: 127.0.0.1:{} on the guest's loopback", host.port);
    }
    if matches!(status, Status::Running | Status::Stopped) {
        let pending = super::update::mount_policy(&info.instance, Path::new(&info.install_dir)).and_then(|shares| {
            Ok(super::update::plan_mounts(&shares, &super::update::read_lima(&info.instance)?.shares()).changes)
        });
        match pending {
            Ok(changes) if !changes.is_empty() => {
                println!("Lima mounts: pending before next start\n  {}", changes.join("\n  "))
            }
            Err(error) => println!("Lima mounts: unknown ({error:#})"),
            _ => {}
        }
    }
    if status == Status::Running {
        let config = super::update::load_config()?;
        let report = super::built::report_with(&info, &config.vm.modules, super::built::probe);
        super::built::say(&mut std::io::stdout(), &report)?;
    }
    Ok(())
}

pub fn start() -> Result<()> {
    let info = installed()?;
    if !super::created_here(&info.instance) && super::lima_status_for_command(&info.instance)?.is_none() {
        bail!("{} does not exist yet; run atomics vm setup", info.instance);
    }
    super::start(&info.instance)?;
    println!("{} is running", info.instance);
    Ok(())
}

pub fn stop() -> Result<()> {
    let info = installed()?;
    super::stop(&info.instance)
}

pub fn check_command() -> Result<()> {
    let info = installed()?;
    check(&info)?;
    say("Sign-ins left");
    sign_ins(&info);
    Ok(())
}

#[cfg(test)]
mod tests;
