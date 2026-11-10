//! Updating this installation: finding a newer version, and installing one so that any failure leaves the old one
//! as it was. A version comes from a signed release, checked against the keys built into this binary before anything
//! is unpacked, or from a checkout the user names ([`source`]). The window and `atomics update` share all of it.
//! Restarting into what was installed is [`crate::app`]'s safe restart.
//!
//! An update replaces the program and nothing else: config, plans, sessions, paired devices and keychain items are not
//! its to touch. What it replaced is kept beside the install, and `atomics rollback` swaps back ([`rollback`]).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use atomics_core::config::Config;
use atomics_core::control::ControlRequest;
use base64::Engine;
use serde::Deserialize;

mod source;
use source::Source;

/// How often the window looks. A check that failed waits its turn too, so nothing retries in a burst.
pub const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
const CHECK_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const STALL_TIMEOUT: Duration = Duration::from_secs(60);
/// More than any real release: a bound on what is read into memory to verify it.
const MAX_ARCHIVE: u64 = 512 * 1024 * 1024;
const MAX_ANSWER: u64 = 4 * 1024 * 1024;
const MAX_SIDECAR: u64 = 4 * 1024;

/// The public keys in `assets/` when this binary was built, if the checkout had them: the one CI signs releases with, and
/// an offline one kept for when that is lost or leaks. A signature from either will do, so that a release signed offline
/// can bring a new CI key to every install. They are the only trust anchors: nothing at run time can name another, and a
/// key that came with a download proves nothing.
pub(crate) const BUILT_IN_KEYS: &[&str] = include!(concat!(env!("OUT_DIR"), "/release_keys.rs"));
/// What a PEM Ed25519 public key decodes to, before the 32 bytes of the key itself.
const ED25519_PUBLIC_KEY_PREFIX: [u8; 12] = [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];
const NO_KEY: &str = "This build has no release signing key, so it cannot check a download and will not install \
                      one. Install the new version by hand, or set [update] source to build from a checkout.";

/// The folders an update unpacks into, beside what it replaces. One left by a crash goes with the next update.
pub(crate) const STAGING: &str = ".atomics-update-";
/// Where a bundle's previous version is kept: not an `.app`, so that macOS offers no second ATOMICS.
const PREVIOUS_BUNDLE: &str = ".atomics-previous";
/// What a program's previous version is called, beside it.
const PREVIOUS_SUFFIX: &str = ".previous";

/// Where new versions come from.
#[derive(Clone)]
pub enum Mode {
    Releases(Feed),
    Source(Source),
}

/// A newer version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub version: String,
    pub detail: Option<String>,
    pub notes: Option<String>,
    /// The release's files for this platform; none from a checkout, or from a release without this platform.
    assets: Option<Assets>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Assets {
    name: String,
    archive: String,
    checksum: String,
    signature: String,
}

impl Mode {
    pub fn of(config: &Config) -> Mode {
        match config.update.source.trim() {
            "" => Mode::Releases(Feed::releases()),
            path => Mode::Source(Source::new(path, &config.resources)),
        }
    }

    /// The version running here, in the terms a new one is compared in.
    pub fn current(&self) -> String {
        match self {
            Mode::Releases(_) => env!("CARGO_PKG_VERSION").into(),
            Mode::Source(_) => crate::restart::version().into(),
        }
    }

    /// The newer version, if there is one.
    pub fn check(&self) -> Result<Option<Found>> {
        match self {
            Mode::Releases(feed) => feed.latest(env!("CARGO_PKG_VERSION")),
            Mode::Source(source) => source.check(),
        }
    }

    /// Why this install cannot take `found`, said before anyone presses anything.
    pub fn unavailable(&self, found: &Found) -> Option<String> {
        let feed = match self {
            Mode::Releases(feed) => feed,
            Mode::Source(source) => return source.unavailable(),
        };
        if let Err(why) = &feed.keys {
            return Some(why.clone());
        }
        if found.assets.is_none() {
            return Some(format!("Release {} has no build for {}.", found.version, feed.platform));
        }
        feed.install_target().err().map(|why| format!("{why:#}"))
    }

    /// Downloads and swaps in `found`, or builds it, saying each step through `progress`.
    pub fn install(&self, found: &Found, progress: &dyn Fn(String)) -> Result<()> {
        match self {
            Mode::Releases(feed) => feed.install(found, progress),
            Mode::Source(source) => source.install(progress),
        }
    }
}

/// Signed releases, as a host reads them from the repository's public API.
#[derive(Clone)]
pub struct Feed {
    /// The latest release's address.
    url: String,
    /// What a release's signature may be from.
    keys: Result<Vec<[u8; 32]>, String>,
    /// This platform, as release archives name it.
    platform: String,
    /// What a release replaces: this installation, unless a test says otherwise.
    target: Option<InstallTarget>,
    /// Off only for a test's server on this machine.
    remote: bool,
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

impl Feed {
    fn releases() -> Feed {
        let repository = env!("CARGO_PKG_REPOSITORY").trim_start_matches("https://github.com/").trim_end_matches('/');
        Feed {
            url: format!("https://api.github.com/repos/{repository}/releases/latest"),
            keys: built_in_keys(BUILT_IN_KEYS),
            platform: env!("ATOMICS_TARGET").into(),
            target: None,
            remote: true,
        }
        .test_feed()
    }

    /// In a debug build only, another feed and key, so that a test can update the real program from a server of its own.
    /// No release build has this: there, nothing at run time can name another key, which CI checks of each release.
    #[cfg(debug_assertions)]
    fn test_feed(self) -> Feed {
        let (Some(url), Some(key)) =
            (std::env::var_os("ATOMICS_TEST_RELEASE_FEED"), std::env::var_os("ATOMICS_TEST_RELEASE_KEY"))
        else {
            return self;
        };
        let keys =
            std::fs::read_to_string(key).map_err(|error| error.to_string()).and_then(|pem| parse_public_key(&pem));
        Feed { url: url.to_string_lossy().into_owned(), keys: keys.map(|key| vec![key]), remote: false, ..self }
    }

    #[cfg(not(debug_assertions))]
    fn test_feed(self) -> Feed {
        self
    }

    fn agent(&self, config: ureq::config::ConfigBuilder<ureq::typestate::AgentScope>) -> ureq::Agent {
        let config = config
            .http_status_as_error(false)
            .https_only(self.remote)
            .user_agent(concat!("atomics/", env!("CARGO_PKG_VERSION")));
        // A test's server is never reached through this machine's proxy.
        let config = if self.remote { config } else { config.proxy(None) };
        config.build().into()
    }

    fn latest(&self, current: &str) -> Result<Option<Found>> {
        let agent = self.agent(ureq::Agent::config_builder().timeout_global(Some(CHECK_TIMEOUT)));
        let mut answer = agent
            .get(&self.url)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
            .call()
            .map_err(|error| anyhow!("Could not reach GitHub: {}", crate::github::transport(&error)))?;
        match answer.status().as_u16() {
            200 => {}
            // No release is published yet.
            404 => return Ok(None),
            403 | 429 => bail!("GitHub is limiting requests from this address."),
            status => bail!("GitHub answered {status}."),
        }
        let body = answer
            .body_mut()
            .with_config()
            .limit(MAX_ANSWER)
            .read_to_vec()
            .map_err(|error| anyhow!("Could not read GitHub's answer: {}", crate::github::transport(&error)))?;
        let release: GitHubRelease =
            serde_json::from_slice(&body).map_err(|_| anyhow!("GitHub's answer could not be read."))?;
        if !is_newer(current, &release.tag_name) {
            return Ok(None);
        }
        let version = release.tag_name.trim_start_matches('v').to_string();
        let name = format!("atomics-{version}-{}.tar.gz", self.platform);
        let url = |name: &str| {
            release.assets.iter().find(|asset| asset.name == name).map(|asset| asset.browser_download_url.clone())
        };
        let assets = match (url(&name), url(&format!("{name}.sha256")), url(&format!("{name}.sig"))) {
            (Some(archive), Some(checksum), Some(signature)) => Some(Assets { name, archive, checksum, signature }),
            _ => None,
        };
        let notes = release.html_url.filter(|url| url.starts_with("https://"));
        Ok(Some(Found { version, detail: None, notes, assets }))
    }

    fn install_target(&self) -> Result<InstallTarget> {
        match &self.target {
            Some(target) => Ok(target.clone()),
            None => InstallTarget::current(),
        }
    }

    /// Nothing is unpacked before the checksum and the signature hold, and nothing is replaced before what was
    /// unpacked runs.
    fn install(&self, found: &Found, progress: &dyn Fn(String)) -> Result<()> {
        let keys = self.keys.as_ref().map_err(|why| anyhow!("{why}"))?;
        let assets = found
            .assets
            .as_ref()
            .ok_or_else(|| anyhow!("Release {} has no build for {}.", found.version, self.platform))?;
        let result = (|| {
            let target = self.install_target()?;
            progress(format!("Downloading {}", found.version));
            let agent = self.agent(
                ureq::Agent::config_builder()
                    .timeout_connect(Some(STALL_TIMEOUT))
                    .timeout_per_read(Some(STALL_TIMEOUT))
                    .timeout_global(Some(DOWNLOAD_TIMEOUT)),
            );
            let archive = download(&agent, &assets.archive, MAX_ARCHIVE, "the release")?;
            let checksum = download(&agent, &assets.checksum, MAX_SIDECAR, "the release's checksum")?;
            let signature = download(&agent, &assets.signature, MAX_SIDECAR, "the release's signature")?;
            progress(format!("Verifying {}", found.version));
            verify(&archive, &assets.name, &checksum, &signature, keys)?;
            progress(format!("Installing {}", found.version));
            target.install(&archive, &found.version)
        })();
        result.map_err(|error| anyhow!("{error:#} The installed version was not changed."))
    }
}

/// `text`, with or without its `v`, as three numbers. Anything else, a pre-release included, is no version to offer.
fn version_numbers(text: &str) -> Option<[u64; 3]> {
    let mut parts = text.strip_prefix('v').unwrap_or(text).split('.').map(str::parse::<u64>);
    let numbers = [parts.next()?.ok()?, parts.next()?.ok()?, parts.next()?.ok()?];
    parts.next().is_none().then_some(numbers)
}

fn is_newer(current: &str, candidate: &str) -> bool {
    matches!((version_numbers(current), version_numbers(candidate)), (Some(current), Some(new)) if new > current)
}

fn download(agent: &ureq::Agent, url: &str, limit: u64, what: &str) -> Result<Vec<u8>> {
    let failed = |error: &ureq::Error| anyhow!("Could not download {what}: {}.", crate::github::transport(error));
    let mut answer = agent.get(url).call().map_err(|error| failed(&error))?;
    if !answer.status().is_success() {
        bail!("Could not download {what}: the server answered {}.", answer.status().as_u16());
    }
    answer.body_mut().with_config().limit(limit).read_to_vec().map_err(|error| failed(&error))
}

/// The raw key from a PEM Ed25519 public key.
fn parse_public_key(pem: &str) -> Result<[u8; 32], String> {
    let bad = || "The release signing key built into this binary cannot be read, so updates are off.".to_string();
    let inner = pem
        .trim()
        .strip_prefix("-----BEGIN PUBLIC KEY-----")
        .and_then(|rest| rest.strip_suffix("-----END PUBLIC KEY-----"))
        .ok_or_else(bad)?;
    let body: String = inner.split_whitespace().collect();
    let der = base64::engine::general_purpose::STANDARD.decode(body).map_err(|_| bad())?;
    der.strip_prefix(&ED25519_PUBLIC_KEY_PREFIX).and_then(|raw| raw.try_into().ok()).ok_or_else(bad)
}

/// Every key in `pems`, or why updates are off: a build with a key it cannot read is a broken build, and guessing which of
/// its keys still counts is no way to treat a trust anchor.
pub(crate) fn built_in_keys(pems: &[&str]) -> Result<Vec<[u8; 32]>, String> {
    if pems.is_empty() {
        return Err(NO_KEY.to_string());
    }
    pems.iter().copied().map(parse_public_key).collect()
}

/// The checksum, then the raw 64-byte Ed25519 signature over the archive's exact bytes, from any one of `keys`.
pub(crate) fn verify(archive: &[u8], name: &str, checksum: &[u8], signature: &[u8], keys: &[[u8; 32]]) -> Result<()> {
    let listed = std::str::from_utf8(checksum)
        .ok()
        .and_then(|text| {
            let mut fields = text.split_whitespace();
            let digest = fields.next()?;
            let file = fields.next()?;
            (fields.next().is_none() && file.trim_start_matches('*') == name).then_some(digest)
        })
        .ok_or_else(|| anyhow!("The release's checksum file names another file, or cannot be read."))?;
    let digest: String = ring::digest::digest(&ring::digest::SHA256, archive)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if !listed.eq_ignore_ascii_case(&digest) {
        bail!("The download is damaged: its SHA-256 does not match the release's.");
    }
    let verified = signature.len() == 64
        && keys.iter().any(|key| {
            ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key).verify(archive, signature).is_ok()
        });
    if !verified {
        bail!("The download is not signed with a release key built into ATOMICS.");
    }
    Ok(())
}

/// What a release replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallTarget {
    /// The installed program, replaced by a rename: a running one keeps its old file, so no ETXTBSY.
    Binary(PathBuf),
    /// The installed `.app`, swapped whole.
    Bundle(PathBuf),
}

impl InstallTarget {
    fn current() -> Result<Self> {
        Self::of(&crate::setup::executable_path().context("locating this installation")?, cfg!(target_os = "macos"))
    }

    fn of(executable: &Path, macos: bool) -> Result<Self> {
        // A link in a bin folder stays a link to the file that gets replaced.
        let resolved = executable.canonicalize().unwrap_or_else(|_| executable.to_path_buf());
        let development = resolved.ancestors().any(|dir| {
            dir.file_name().is_some_and(|name| name == "target")
                && dir.parent().is_some_and(|parent| parent.join("Cargo.toml").is_file())
        });
        if macos {
            return match crate::restart::bundle_of(&resolved) {
                Some(bundle) => Ok(Self::Bundle(bundle.to_path_buf())),
                None => bail!("ATOMICS is not running from an app bundle, so there is nothing to swap."),
            };
        }
        if development {
            bail!(
                "This is a development build at {}. Build it again, or set [update] source to build from a checkout.",
                resolved.display()
            );
        }
        Ok(Self::Binary(resolved))
    }

    fn path(&self) -> &Path {
        match self {
            Self::Binary(path) | Self::Bundle(path) => path,
        }
    }

    /// Where what this replaces is kept. A program's previous version sits beside it, and a bundle's under a name macOS
    /// does not offer as an app.
    pub(crate) fn previous(&self) -> PathBuf {
        match self {
            Self::Binary(path) => {
                let mut name = path.file_name().unwrap_or_default().to_os_string();
                name.push(PREVIOUS_SUFFIX);
                path.with_file_name(name)
            }
            Self::Bundle(path) => path.with_file_name(PREVIOUS_BUNDLE),
        }
    }

    /// The install this is, or the install this is the kept previous version of.
    fn of_rollback(executable: &Path, macos: bool) -> Result<Self> {
        let resolved = executable.canonicalize().unwrap_or_else(|_| executable.to_path_buf());
        if resolved.file_name().is_some_and(|name| name.to_string_lossy() == format!("atomics{PREVIOUS_SUFFIX}")) {
            return Ok(Self::Binary(resolved.with_file_name("atomics")));
        }
        if let Some(kept) =
            resolved.ancestors().nth(3).filter(|kept| kept.file_name().is_some_and(|n| n == PREVIOUS_BUNDLE))
        {
            return Ok(Self::Bundle(kept.with_file_name("ATOMICS.app")));
        }
        Self::of(&resolved, macos)
    }

    /// Unpacks beside what it replaces, on the same filesystem, so the last step is one rename.
    fn install(&self, archive: &[u8], version: &str) -> Result<()> {
        let target = self.path();
        let parent = target.parent().context("the install has no folder")?;
        remove_stale_staging(parent);
        let staging = tempfile::Builder::new()
            .prefix(STAGING)
            .tempdir_in(parent)
            .with_context(|| format!("Could not write to {}", parent.display()))?;
        let download = staging.path().join("download.tar.gz");
        std::fs::write(&download, archive).with_context(|| format!("Could not write to {}", parent.display()))?;
        let unpacked = staging.path().join("unpacked");
        std::fs::create_dir(&unpacked).with_context(|| format!("Could not write to {}", parent.display()))?;
        unpack(&download, &unpacked)?;
        let new = match self {
            Self::Binary(_) => unpacked.join("atomics"),
            Self::Bundle(_) => unpacked.join("ATOMICS.app"),
        };
        must_report_in(self, &new, version)?;
        place(self, &new, staging.path())
    }
}

/// Refuses an archive whose program does not run, or does not name `version`.
fn must_report_in(target: &InstallTarget, new: &Path, version: &str) -> Result<()> {
    let program = match target {
        InstallTarget::Binary(_) => new.to_path_buf(),
        InstallTarget::Bundle(_) => new.join("Contents/MacOS/atomics"),
    };
    must_run(&program)?;
    must_report(&program, version)
}

/// Puts `new`, staged in `staging` on the install's filesystem, where `target` says, keeping what was there. It must run
/// here first. The name stays taken throughout where the system can do that, and everything is on the disk before it
/// goes under the real name, so that a power cut finds one version or the other.
pub(crate) fn place(target: &InstallTarget, new: &Path, staging: &Path) -> Result<()> {
    let path = target.path();
    let parent = path.parent().context("the install has no folder")?;
    let program = match target {
        InstallTarget::Binary(_) => new.to_path_buf(),
        InstallTarget::Bundle(_) => new.join("Contents/MacOS/atomics"),
    };
    must_run(&program)?;
    on_disk(new).with_context(|| format!("Could not write to {}", parent.display()))?;
    match target {
        InstallTarget::Binary(path) => {
            keep_previous(path, &target.previous(), staging).and_then(|()| std::fs::rename(new, path))
        }
        InstallTarget::Bundle(path) => replace_bundle(new, path, &target.previous(), staging),
    }
    .with_context(|| format!("Could not replace {}", path.display()))?;
    // The swap is done and stands; this only makes it last through a power cut, where a folder can sync at all.
    let _ = std::fs::File::open(parent).and_then(|folder| folder.sync_all());
    Ok(())
}

/// Keeps the program at `path` under `previous`, with the name it has now staying in place: another name for the same
/// file, or a copy where a link cannot be made. The file a running program holds is the one kept, unchanged.
fn keep_previous(path: &Path, previous: &Path, staging: &Path) -> std::io::Result<()> {
    if !std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file()) {
        return Ok(());
    }
    let kept = staging.join("kept");
    if std::fs::hard_link(path, &kept).is_err() {
        std::fs::copy(path, &kept)?;
        std::fs::File::open(&kept)?.sync_all()?;
    }
    std::fs::rename(&kept, previous)
}

/// Swaps the bundle `new` in, and moves what was there to `previous`, in place of what was kept before.
fn replace_bundle(new: &Path, target: &Path, previous: &Path, staging: &Path) -> std::io::Result<()> {
    if !target.exists() {
        return std::fs::rename(new, target);
    }
    let aside = staging.join("previous.app");
    swap_bundle(new, target, &aside)?;
    // The swap stands whatever happens to the copy kept.
    let old = if new.exists() { new } else { aside.as_path() };
    let _ = std::fs::remove_dir_all(previous);
    let _ = std::fs::rename(old, previous);
    Ok(())
}

/// Puts `path` on the disk, all of it where it is a folder, so that a power cut after the rename finds the new program
/// and not an empty file under its name. A link's target is inside what gets synced, and a folder that cannot sync
/// only risks its own names.
pub(crate) fn on_disk(path: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_symlink() {
        return Ok(());
    }
    if meta.is_dir() {
        for entry in std::fs::read_dir(path)? {
            on_disk(&entry?.path())?;
        }
        let _ = std::fs::File::open(path).and_then(|folder| folder.sync_all());
        return Ok(());
    }
    std::fs::File::open(path)?.sync_all()
}

/// With the update's lock held, whatever an earlier one left is a crash's.
fn remove_stale_staging(parent: &Path) {
    for entry in std::fs::read_dir(parent).into_iter().flatten().flatten() {
        if entry.file_name().to_string_lossy().starts_with(STAGING) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// The archive's owner is the build's, and means nothing here: with `sudo atomics update`, tar would otherwise give the
/// installed program to whoever has that number on this machine.
fn unpack_command(archive: &Path, folder: &Path) -> Command {
    let mut tar = Command::new("tar");
    tar.arg("--no-same-owner").arg("-xzf").arg(archive).arg("-C").arg(folder);
    tar
}

fn unpack(archive: &Path, folder: &Path) -> Result<()> {
    let output = unpack_command(archive, folder).stdin(Stdio::null()).output().context("Could not run tar")?;
    if !output.status.success() {
        let why = String::from_utf8_lossy(&output.stderr);
        bail!("The release could not be unpacked: {}", why.lines().next().unwrap_or("tar failed"));
    }
    Ok(())
}

/// Refuses what would leave a broken install behind: a program that is missing, or does not run here.
pub(crate) fn must_run(program: &Path) -> Result<()> {
    if !std::fs::symlink_metadata(program).is_ok_and(|meta| meta.is_file()) {
        bail!("The release does not hold {}.", program.file_name().unwrap_or_default().to_string_lossy());
    }
    crate::restart::Target { executable: program.to_path_buf(), command: Vec::new() }
        .validate()
        .map_err(|why| anyhow!("The new version does not run on this machine: {why:#}."))
}

/// Refuses an older build offered under a newer tag. The signature covers the archive's bytes and says nothing of
/// its version, so a genuinely signed old release would pass it; the program itself has to name the version.
fn must_report(program: &Path, version: &str) -> Result<()> {
    let output = crate::restart::version_output(program)
        .map_err(|why| anyhow!("The new version does not run on this machine: {why:#}."))?;
    let reported = reported_version(&output);
    if reported != Some(version) {
        bail!(
            "Release {version} holds a program that reports {}, so it is not the version it claims to be.",
            reported.unwrap_or("no version")
        );
    }
    Ok(())
}

/// The version in what `atomics --version` prints: `atomics 0.1.7 (<build>)`.
fn reported_version(output: &str) -> Option<&str> {
    output.lines().next()?.strip_prefix("atomics ")?.split_whitespace().next()
}

/// Puts the folder `new` at `target`, the old one at `new` or `aside`. Where the system can swap two names at once
/// the bundle is never missing; otherwise it is for one rename, and put back if the second fails.
fn swap_bundle(new: &Path, target: &Path, aside: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    if exchange(new, target).is_ok() {
        return Ok(());
    }
    std::fs::rename(target, aside)?;
    std::fs::rename(new, target).inspect_err(|_| {
        let _ = std::fs::rename(aside, target);
    })
}

/// `first` and `second` trade names at once, where the system can.
#[cfg(target_os = "linux")]
fn exchange(first: &Path, second: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let first = std::ffi::CString::new(first.as_os_str().as_bytes())?;
    let second = std::ffi::CString::new(second.as_os_str().as_bytes())?;
    const RENAME_EXCHANGE: libc::c_uint = 2;
    // SAFETY: both are valid NUL-terminated paths for the duration of the call.
    let done = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            first.as_ptr(),
            libc::AT_FDCWD,
            second.as_ptr(),
            RENAME_EXCHANGE,
        )
    };
    if done != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn exchange(first: &Path, second: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let first = std::ffi::CString::new(first.as_os_str().as_bytes())?;
    let second = std::ffi::CString::new(second.as_os_str().as_bytes())?;
    // SAFETY: both are valid NUL-terminated paths for the duration of the call.
    if unsafe { libc::renamex_np(first.as_ptr(), second.as_ptr(), libc::RENAME_SWAP) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Held while an update runs, here or in a terminal: the system lets it go with the process.
pub struct Lock(#[allow(dead_code)] std::fs::File);

pub fn lock(home: &Path) -> Result<Lock> {
    use std::os::fd::AsRawFd;
    std::fs::create_dir_all(home).with_context(|| format!("Could not write to {}", home.display()))?;
    let path = home.join("update.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("Could not write to {}", path.display()))?;
    // SAFETY: flock on the descriptor opened above.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("Another update is under way.");
    }
    Ok(Lock(file))
}

/// `previous` and `target` trade places, so that doing this again goes forward again. Where the system cannot trade two
/// names at once, three renames do it and put things back if one fails.
fn trade_places(previous: &Path, target: &Path, aside: &Path) -> std::io::Result<()> {
    if exchange(previous, target).is_ok() {
        return Ok(());
    }
    if !target.exists() {
        return std::fs::rename(previous, target);
    }
    std::fs::rename(target, aside)?;
    if let Err(error) = std::fs::rename(previous, target) {
        let _ = std::fs::rename(aside, target);
        return Err(error);
    }
    std::fs::rename(aside, previous).inspect_err(|_| {
        let _ = std::fs::rename(target, previous);
        let _ = std::fs::rename(aside, target);
    })
}

/// Goes back to the version the last update replaced, from either of the two: the kept copy can run this when the new
/// one will not start at all. What the update replaced has to run here. Doing it again goes forward again.
pub fn rollback(executable: &Path, macos: bool) -> Result<String> {
    let target = InstallTarget::of_rollback(executable, macos)?;
    let previous = target.previous();
    if !previous.exists() {
        bail!("No previous version is kept at {}: an update keeps the one it replaces.", previous.display());
    }
    let program = match &target {
        InstallTarget::Binary(_) => previous.clone(),
        InstallTarget::Bundle(_) => previous.join("Contents/MacOS/atomics"),
    };
    must_run(&program)?;
    let version = crate::restart::version_output(&program)
        .ok()
        .and_then(|output| reported_version(&output).map(str::to_string))
        .unwrap_or_else(|| "the previous version".into());
    let parent = target.path().parent().context("the install has no folder")?;
    let aside = parent.join(format!("{STAGING}rollback-{}", std::process::id()));
    trade_places(&previous, target.path(), &aside)
        .with_context(|| format!("Could not swap {} and {}", previous.display(), target.path().display()))?;
    let _ = std::fs::File::open(parent).and_then(|folder| folder.sync_all());
    Ok(version)
}

/// `atomics rollback`.
pub fn run_rollback_cli() -> i32 {
    // An update under way holds this too: the two must not trade places under each other.
    let result = lock(&atomics_core::paths::atomics_home()).and_then(|_lock| {
        let executable = crate::setup::executable_path().context("locating this installation")?;
        rollback(&executable, cfg!(target_os = "macos"))
    });
    match result {
        Ok(version) => {
            println!("ATOMICS {version} is back in place. Running this again goes forward again.");
            println!("A running ATOMICS keeps the version it started with until it restarts.");
            0
        }
        Err(error) => {
            eprintln!("atomics rollback: {error:#}");
            1
        }
    }
}

/// `atomics update [--check]`. The check asks whatever `[update] check` says: someone asked.
pub fn run_cli(check_only: bool) -> i32 {
    match cli(check_only) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("atomics update: {error:#}");
            1
        }
    }
}

fn cli(check_only: bool) -> Result<()> {
    let path = atomics_core::paths::config_file();
    let config =
        if path.exists() { Config::load(&path).context("Could not read the config")? } else { Config::default() };
    let mode = Mode::of(&config);
    let current = mode.current();
    let Some(found) = mode.check()? else {
        println!("ATOMICS {current} is up to date.");
        return Ok(());
    };
    println!("ATOMICS {} is available; this is {current}.", found.version);
    for line in [&found.detail, &found.notes].into_iter().flatten() {
        println!("{line}");
    }
    let unavailable = mode.unavailable(&found);
    if check_only {
        if let Some(why) = unavailable {
            println!("{why}");
        }
        return Ok(());
    }
    if let Some(why) = unavailable {
        bail!("{why}");
    }
    let _lock = lock(&atomics_core::paths::atomics_home())?;
    mode.install(&found, &|step| println!("{step}"))?;
    println!("Installed {}.", found.version);
    let program = crate::setup::executable_path().ok().map(|path| path.to_string_lossy().into_owned());
    let request = ControlRequest::UpdateInstalled { version: found.version, program };
    match crate::ipc::request_at(&atomics_core::paths::socket_path(), &request) {
        Ok(answer) if answer.ok => println!("ATOMICS restarts when agents finish; its Update button can cancel that."),
        Ok(answer) => println!("{}", answer.error.unwrap_or_default()),
        Err(crate::ipc::RequestError::NotRunning(_)) => println!("The next start of ATOMICS runs it."),
        Err(crate::ipc::RequestError::Failed(error)) => {
            println!("The running ATOMICS did not answer ({error:#}); restart it to use the new version.");
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;
