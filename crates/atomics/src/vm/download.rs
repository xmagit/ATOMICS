//! Signed, version-bound VM releases. The bundle's files come from mac-desktop.nix; this module
//! fills their identity placeholder and size fields, and never reconstructs wrappers in Rust.
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use super::{BuildInfo, BundleInfo, Flavor};

const GIB: u64 = 1024 * 1024 * 1024;
const IMAGE_BYTES: u64 = 100 * GIB;
const MAX_COMPRESSED: u64 = 16 * GIB;
const MAX_TEMPLATE: u64 = 1024 * 1024;
const USER_TOKEN: &str = "@ATOMICS_MAC_USER@";

#[derive(Default, clap::Args)]
pub struct Sizes {
    /// Virtual CPUs (default: this Mac's cores minus four, at least one)
    #[arg(long, conflicts_with = "from", value_parser = clap::value_parser!(u32).range(1..))]
    pub cpus: Option<u32>,
    /// VM memory in GiB (default: half this Mac's memory, at least one)
    #[arg(long, conflicts_with = "from", value_parser = clap::value_parser!(u32).range(1..))]
    pub memory_gib: Option<u32>,
    /// Lima disk in GiB (default: 100; cannot be smaller than the release image)
    #[arg(long, conflicts_with = "from", value_parser = clap::value_parser!(u32).range(100..))]
    pub disk_gib: Option<u32>,
}

impl Sizes {
    fn resolve(&self, cores: u32, memory_bytes: u64) -> Result<BuildInfo> {
        let build = BuildInfo {
            cpus: self.cpus.unwrap_or(cores.saturating_sub(4).max(1)),
            memory_gib: self.memory_gib.unwrap_or((memory_bytes / GIB / 2).clamp(1, u64::from(u32::MAX)) as u32),
            disk_gib: self.disk_gib.unwrap_or(100),
        };
        if build.cpus == 0 || build.memory_gib == 0 || build.disk_gib < 100 {
            bail!("VM sizes must be positive, and the disk must be at least 100 GiB");
        }
        Ok(build)
    }

    fn this_mac(&self) -> Result<BuildInfo> {
        fn sysctl(name: &str) -> Result<u64> {
            let output = std::process::Command::new("/usr/sbin/sysctl").args(["-n", name]).output()?;
            if !output.status.success() {
                bail!("could not read this Mac's {name}");
            }
            Ok(std::str::from_utf8(&output.stdout)?.trim().parse()?)
        }
        self.resolve(sysctl("hw.logicalcpu")?.try_into()?, sysctl("hw.memsize")?)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Template {
    version: u32,
    #[serde(default)]
    flavor: Flavor,
    release_version: String,
    image: String,
    image_sha256: String,
    image_bytes: u64,
    files: BTreeMap<String, TemplateFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TemplateFile {
    contents: String,
    executable: bool,
}

impl Template {
    fn read(bytes: &[u8], version: &str, flavor: Flavor) -> Result<Self> {
        let template: Self = serde_json::from_slice(bytes).context("reading the VM bundle template")?;
        if template.version != 1
            || template.flavor != flavor
            || template.release_version != version
            || template.image != format!("{}.raw.zst", flavor.asset_prefix(version))
            || template.image_bytes != IMAGE_BYTES
            || template.image_sha256.len() != 64
            || !template.image_sha256.bytes().all(|b| b.is_ascii_hexdigit())
        {
            bail!("the signed VM bundle does not describe this ATOMICS version's image");
        }
        Ok(template)
    }

    fn fill(&self, bundle: &Path, user: &str, sizes: &BuildInfo) -> Result<()> {
        // One substitution enters shell, JSON, TOML and XML. Restrict it to their common safe alphabet.
        if user.is_empty()
            || !user.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            || user.starts_with('.')
            || user.starts_with('-')
            || user.contains("..")
        {
            bail!("the Mac account name cannot be represented safely in this bundle");
        }
        for required in ["install.sh", "lima.yaml", super::BUNDLE_INFO, "config.d.toml", "bin/atomics-vm-host-forward"]
        {
            if !self.files.contains_key(required) {
                bail!("the VM bundle template lacks {required}");
            }
        }
        // Validate the complete manifest before writing any files. No archive links or traversal.
        for name in self.files.keys() {
            if Path::new(name).components().any(|c| !matches!(c, Component::Normal(_))) || name.is_empty() {
                bail!("invalid bundle template path: {name}");
            }
        }
        std::fs::create_dir(bundle)?;
        for (name, file) in &self.files {
            let mut contents = file.contents.replace(USER_TOKEN, user);
            if contents.contains("@ATOMICS_") {
                bail!("unresolved placeholder in {name}");
            }
            if name == "lima.yaml" {
                // The generic Nix bundle emits JSON, a YAML subset, so no YAML parser is needed here.
                let mut lima: serde_json::Value = serde_json::from_str(&contents)?;
                lima["cpus"] = sizes.cpus.into();
                lima["memory"] = format!("{}GiB", sizes.memory_gib).into();
                lima["disk"] = format!("{}GiB", sizes.disk_gib).into();
                contents = serde_json::to_string_pretty(&lima)?;
            } else if name == super::BUNDLE_INFO {
                let mut info: BundleInfo = serde_json::from_str(&contents)?;
                info.build = Some(sizes.clone());
                if info.instance != "atomics"
                    || info.mac_user != user
                    || info.host.is_none()
                    || info.flavor != self.flavor
                {
                    bail!("the release bundle must name this Mac account and enable the VM host");
                }
                contents = serde_json::to_string_pretty(&info)?;
            }
            let path = bundle.join(name);
            std::fs::create_dir_all(path.parent().context("bundle file has no parent")?)?;
            std::fs::write(&path, contents)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(
                    path,
                    std::fs::Permissions::from_mode(if file.executable { 0o755 } else { 0o644 }),
                )?;
            }
        }
        BundleInfo::read(bundle)?;
        Ok(())
    }
}

/// Downloads into a private staging directory. Both assets must pass the updater's exact-byte
/// checksum and Ed25519 verification before any bundle script is run or any disk is decompressed.
fn download_asset(
    agent: &ureq::Agent,
    base: &str,
    name: &str,
    directory: &Path,
    limit: u64,
    keys: &[[u8; 32]],
    progress: &dyn Fn(String),
) -> Result<File> {
    progress(format!("Downloading {name}"));
    let sidecar = |suffix: &str| -> Result<Vec<u8>> {
        Ok(agent.get(format!("{base}/{name}.{suffix}")).call()?.body_mut().with_config().limit(4096).read_to_vec()?)
    };
    let checksum = sidecar("sha256").with_context(|| format!("missing or unreadable checksum for {name}"))?;
    let signature = sidecar("sig").with_context(|| format!("missing or unreadable signature for {name}"))?;
    let mut response = agent.get(format!("{base}/{name}")).call().with_context(|| {
        format!("downloading {name}; this release must publish its signed VM assets before setup can use it")
    })?;
    let mut reader = response.body_mut().as_reader().take(limit + 1);
    let mut file = tempfile::tempfile_in(directory)?;
    let mut buffer = vec![0; 1024 * 1024];
    let mut total = 0_u64;
    let mut reported = Instant::now();
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > limit {
            bail!("{name} exceeds the download size limit");
        }
        file.write_all(&buffer[..n])?;
        if reported.elapsed() >= Duration::from_secs(2) {
            progress(format!("{name}: {} MiB downloaded", total / 1024 / 1024));
            reported = Instant::now();
        }
    }
    if total == 0 {
        bail!("{name} is empty");
    }
    file.flush()?;
    progress(format!("Verifying {name}"));
    // SAFETY: this unnamed file is owned solely here, in our private staging directory. No writer
    // can mutate or truncate it while the read-only mapping is alive. The mapping is dropped here.
    let bytes = unsafe { memmap2::Mmap::map(&file)? };
    crate::update::verify(&bytes, name, &checksum, &signature, keys)?;
    drop(bytes);
    file.rewind()?;
    Ok(file)
}

/// Stream decompression with zero blocks represented as holes, including a trailing hole. A hard
/// output bound also prevents a signed but malformed zstd frame from exhausting the host disk.
fn decompress(source: File, target: &Path, expected: u64, progress: &dyn Fn(String)) -> Result<()> {
    let mut reader = zstd::stream::read::Decoder::new(source)?;
    let mut file = File::create_new(target)?;
    #[cfg(target_os = "macos")]
    let block_size = {
        use std::os::fd::AsRawFd;
        let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: the descriptor is live and stat points to a correctly sized output structure.
        if unsafe { libc::fstatfs(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: fstatfs succeeded and initialized the structure.
        u64::from(unsafe { stat.assume_init() }.f_bsize)
    };
    let mut buffer = vec![0; 1024 * 1024];
    let mut total = 0_u64;
    let mut reported = Instant::now();
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > expected {
            bail!("the VM image decompresses beyond its declared disk size");
        }
        if buffer[..n].iter().all(|&b| b == 0) {
            file.seek(SeekFrom::Current(n as i64))?;
            #[cfg(target_os = "macos")]
            {
                // APFS can allocate skipped extents when the file is flushed. Punch complete
                // filesystem blocks as we go, so even a large trailing hole stays sparse without
                // temporarily allocating the whole disk. Boundary fragments remain zero-filled.
                file.set_len(total)?;
                punch_hole(&file, total - n as u64, total, block_size)?;
            }
        } else {
            file.write_all(&buffer[..n])?;
        }
        if reported.elapsed() >= Duration::from_secs(2) {
            progress(format!("Decompressing VM disk: {} / {} GiB", total / GIB, expected / GIB));
            reported = Instant::now();
        }
    }
    if total != expected {
        bail!("the decompressed VM image has the wrong disk size");
    }
    file.set_len(total)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn punch_hole(file: &File, start: u64, end: u64, block: u64) -> Result<()> {
    use std::os::fd::AsRawFd;
    let offset = start.div_ceil(block) * block;
    let end = end / block * block;
    if end <= offset {
        return Ok(());
    }
    let hole = libc::fpunchhole_t {
        fp_flags: 0,
        reserved: 0,
        fp_offset: offset as libc::off_t,
        fp_length: (end - offset) as libc::off_t,
    };
    // SAFETY: the descriptor is live and F_PUNCHHOLE reads this correctly initialized structure.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PUNCHHOLE, &hole) } != 0 {
        return Err(std::io::Error::last_os_error()).context("keeping the VM disk sparse (an APFS volume is required)");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn fetch(
    base: &str,
    version: &str,
    flavor: Flavor,
    directory: &Path,
    user: &str,
    sizes: &BuildInfo,
    keys: &[[u8; 32]],
    progress: &dyn Fn(String),
) -> Result<()> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(60)))
        .timeout_per_read(Some(Duration::from_secs(60)))
        .timeout_global(Some(Duration::from_secs(2 * 60 * 60)))
        .build()
        .into();
    let name = format!("{}.bundle.json", flavor.asset_prefix(version));
    let mut manifest = download_asset(&agent, base, &name, directory, MAX_TEMPLATE, keys, progress)?;
    let mut bytes = Vec::new();
    manifest.read_to_end(&mut bytes)?;
    let template = Template::read(&bytes, version, flavor)?;
    let image = download_asset(&agent, base, &template.image, directory, MAX_COMPRESSED, keys, progress)?;
    // Bind the separately signed image to the version in the signed template: an older validly
    // signed image offered under this version's URL must not be accepted.
    let bytes = unsafe { memmap2::Mmap::map(&image)? }; // Same privately owned, immutable file as above.
    let digest: String =
        ring::digest::digest(&ring::digest::SHA256, &bytes).as_ref().iter().map(|b| format!("{b:02x}")).collect();
    if !digest.eq_ignore_ascii_case(&template.image_sha256) {
        bail!("the signed VM image does not match this release's signed bundle");
    }
    drop(bytes);
    progress("Decompressing the VM disk (keeping empty space sparse)".into());
    decompress(image, &directory.join("atomics-vm.raw"), template.image_bytes, progress)?;
    template.fill(&directory.join("bundle"), user, sizes)?;
    Ok(())
}

/// Keep staging alive through Lima's creation and first boot; it owns a disk copy after that.
pub fn prepare(sizes: &Sizes, flavor: Flavor, progress: &dyn Fn(String)) -> Result<tempfile::TempDir> {
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        bail!("Downloading the ATOMICS VM requires an Apple Silicon Mac");
    }
    let sizes = if super::created_here("atomics") && super::lima_status("atomics")?.is_some() {
        progress("Keeping the existing instance's sizes; size flags apply when creating a VM".into());
        super::update::read_lima("atomics")?.sizes()?
    } else {
        sizes.this_mac()?
    };
    let account = std::process::Command::new("/usr/bin/id").arg("-un").output()?;
    if !account.status.success() {
        bail!("could not read the Mac account name");
    }
    let user = std::str::from_utf8(&account.stdout)?.trim();
    let expected_home = format!("/Users/{user}");
    if atomics_core::paths::home() != Path::new(&expected_home) {
        bail!(
            "the release bundle requires a Mac home at /Users/<account>; use a custom --from bundle for another home"
        );
    }
    let version = env!("CARGO_PKG_VERSION");
    let (base, keys) = release_source(version)?;
    let directory = tempfile::Builder::new().prefix("atomics-vm-download-").tempdir()?;
    progress(format!("VM sizes: {} CPUs, {} GiB memory, {} GiB disk", sizes.cpus, sizes.memory_gib, sizes.disk_gib));
    fetch(&base, version, flavor, directory.path(), user, &sizes, &keys, progress)
        .with_context(|| format!("the {} VM download failed; no other image was used", flavor.name()))?;
    Ok(directory)
}

fn release_source(version: &str) -> Result<(String, Vec<[u8; 32]>)> {
    // Like the app updater's fixture feed, this path does not exist in release binaries. It lets
    // isolated QA verify a locally signed image before a branch has published any release assets.
    #[cfg(debug_assertions)]
    if let (Some(base), Some(key)) =
        (std::env::var_os("ATOMICS_TEST_VM_RELEASE_BASE"), std::env::var_os("ATOMICS_TEST_RELEASE_KEY"))
    {
        let pem = std::fs::read_to_string(key)?;
        let keys = crate::update::built_in_keys(&[&pem]).map_err(|e| anyhow!(e))?;
        return Ok((base.to_string_lossy().into_owned(), keys));
    }
    Ok((
        format!("https://github.com/xmagit/ATOMICS/releases/download/v{version}"),
        crate::update::built_in_keys(crate::update::BUILT_IN_KEYS).map_err(|e| anyhow!(e))?,
    ))
}

/// A retained, verified download for the setup screen's separate download and create steps.
pub fn save(hardened: bool, to: &Path) -> Result<()> {
    if to.exists() {
        bail!("{} already exists; choose a new download folder", to.display());
    }
    let flavor = if hardened { Flavor::Hardened } else { Flavor::Standard };
    let prepared = prepare(&Sizes::default(), flavor, &|message| println!("{message}"))?;
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(prepared.path(), to)
        .context("keeping the verified download; use a folder on the same disk as temporary files")?;
    println!("Verified {} image and bundle saved in {}", flavor.name(), to.display());
    Ok(())
}

#[cfg(test)]
mod tests;
