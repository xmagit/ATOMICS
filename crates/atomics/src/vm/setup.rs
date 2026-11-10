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
    let status = std::process::Command::new("/bin/sh")
        .arg(bundle.join("install.sh"))
        .stdout(Stdio::null())
        .status()
        .context("running the bundle's install.sh")?;
    if !status.success() {
        bail!("the bundle's install.sh failed");
    }
    println!("Installed in {} (instance {}, VM user {})", info.install_dir, info.instance, info.user);
    let installed = PathBuf::from(&info.install_dir);

    say(&format!("The Lima instance {}", info.instance));
    let mut created = false;
    match super::lima_status_for_command(&info.instance)? {
        Some(state) if super::created_here(&info.instance) => {
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
            create(&info, &installed, &image)?;
            created = true;
        }
        None => {
            create(&info, &installed, &image)?;
            created = true;
        }
    }

    say("Start");
    match super::status(&info.instance, false) {
        Status::Running => println!("{} is running", info.instance),
        _ => {
            println!("Starting {} (waits for a QA slot)", info.instance);
            super::start(&info.instance)?;
        }
    }

    say("Check the guest");
    check(&info)?;

    let config = super::update::load_config()?;
    if created && !config.vm.modules.is_empty() {
        say("Apply your NixOS modules");
        super::update::run(&installed, &info, &config, super::update::Update { yes: false, no_restart: false })?;
        check(&info)?;
    }

    say("Sign-ins left");
    sign_ins(&info);

    say("This Mac's ATOMICS config");
    match propose_config(&installed)? {
        Some(proposal) => show(&proposal),
        None => println!("Nothing to change."),
    }
    Ok(())
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

/// scripts/check-atomics-vm.sh in the guest, through the QA queue.
pub fn check(info: &BundleInfo) -> Result<()> {
    let limactl = super::limactl().context("limactl is not installed")?;
    let local = tempfile::tempdir()?;
    std::fs::write(local.path().join("check.sh"), CHECK_SCRIPT)?;
    std::fs::write(local.path().join("sshd.sh"), "set -eu\nsudo sshd -T\n")?;
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
fn check_runner(info: &BundleInfo, limactl: &Path, local: &Path) -> Result<Command> {
    #[cfg(test)]
    crate::test_support::assert_fixture_program("limactl", limactl);
    let system = super::system_shell(info)?;
    let mut runner = Command::new("/bin/sh");
    runner.args([
        "-c",
        r#"dir=$1 lima=$2 home=$3 instance=$4 hardened=$5; shift 5
"$@" < "$dir/sshd.sh" > "$dir/sshd-config" || exit $?
exec "$lima" shell --workdir="$home" "$instance" bash --login -c 'exec bash -s -- "$@"' atomics-vm-check --sshd-config "$(cat "$dir/sshd-config")" --hardened "$hardened" < "$dir/check.sh""#,
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
    let Some((installed, _)) = BundleInfo::installed() else {
        bail!("no VM bundle is installed; run atomics vm setup first")
    };
    let Some(proposal) = propose_config(&installed)? else {
        println!("The host config file already has the bundle's settings.");
        return Ok(());
    };
    if !apply {
        show(&proposal);
        return Ok(());
    }
    print!("{}", proposal.diff);
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    println!("{}", apply_config(&proposal, stamp)?);
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
