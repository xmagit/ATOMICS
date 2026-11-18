//! Materialize verification's LFS files from a private copy of the required objects.
use super::*;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;

pub struct VerificationLfs {
    settings: Vec<String>,
}

impl VerificationLfs {
    fn command(&self, checkout: &Path, args: &[&str]) -> Result<host::Invocation> {
        let mut options: Vec<_> = self.settings.iter().flat_map(|s| ["-c", s]).collect();
        options.extend_from_slice(args);
        let mut cmd = command(checkout, &options)?;
        cmd.env("GIT_LFS_SKIP_SMUDGE", "0").env("GIT_LFS_SKIP_DOWNLOAD_ERRORS", "0");
        Ok(cmd)
    }

    pub fn clean(&self, checkout: &Path) -> Result<bool> {
        let args = ["diff", "--name-only", "HEAD"];
        Ok(run(self.command(checkout, &args)?, &args, None, SLOW)?.is_empty())
    }
}

/// Called on the newly detached, unfiltered checkout, before any verifier starts.
/// Never mount or link the source repository's LFS storage into verification.
pub fn materialize_verification_lfs(repo: &Path, checkout: &Path) -> Result<VerificationLfs> {
    let args = ["ls-files", "-z"];
    let files = output(command(checkout, &args)?, &args, None, SLOW)?;
    ensure!(files.status.success(), "cannot enumerate verification files");
    let args = ["check-attr", "--cached", "-z", "--stdin", "filter"];
    let attrs = output(command(checkout, &args)?, &args, Some(&files.stdout), SLOW)?;
    ensure!(attrs.status.success(), "cannot read verification attributes");
    let mut fields = attrs.stdout.split(|&b| b == 0);
    let root = staging::open_root(checkout)?;
    let mut pointers = Vec::new();
    while let Some(name) = fields.next().filter(|p| !p.is_empty()) {
        ensure!(fields.next() == Some(b"filter"), "invalid verification attributes");
        if fields.next().context("missing filter attribute")? != b"lfs" {
            continue;
        }
        let path = Path::new(std::ffi::OsStr::from_bytes(name));
        let staging::Entry::File(file) = staging::entry(&root, path)? else { continue };
        let mut bytes = Vec::new();
        file.take(1025).read_to_end(&mut bytes)?;
        if !bytes.starts_with(b"version https://git-lfs.github.com/spec/v1\n") {
            continue;
        }
        let pointer = std::str::from_utf8(&bytes).context("invalid LFS pointer")?;
        let oid = pointer.lines().find_map(|line| line.strip_prefix("oid sha256:")).context("missing LFS object id")?;
        ensure!(oid.len() == 64 && oid.bytes().all(|b| b.is_ascii_hexdigit()), "invalid LFS object id");
        let size: u64 =
            pointer.lines().find_map(|line| line.strip_prefix("size ")).context("missing LFS size")?.parse()?;
        pointers.push((path, oid.to_owned(), size));
    }
    if pointers.is_empty() {
        return Ok(VerificationLfs { settings: Vec::new() });
    }
    let lfs = host::installed_program("git-lfs").context("verification needs host-installed Git LFS")?;
    let args = ["config", "--path", "--get", "lfs.storage"];
    let out = output(command(repo, &args)?, &args, None, quick())?;
    ensure!(matches!(out.status.code(), Some(0 | 1)), "cannot locate Git LFS storage");
    let configured = std::str::from_utf8(&out.stdout)?.trim_end_matches('\n');
    let source = common_dir(repo)?.join(if configured.is_empty() { "lfs" } else { configured }).join("objects");
    let source = staging::open_root(&source).context("local LFS object store is missing or inaccessible")?;
    let storage = common_dir(checkout)?.join("lfs");
    let mut paths = Vec::new();
    for (path, oid, size) in &pointers {
        let object = Path::new(&oid[..2]).join(&oid[2..4]).join(oid);
        let dest = storage.join("objects").join(&object);
        if !dest.exists() {
            let staging::Entry::File(mut file) = staging::entry(&source, &object)? else {
                bail!("missing local LFS object {oid} for {}", path.display())
            };
            ensure!(file.metadata()?.len() == *size, "local LFS object {oid} has the wrong size");
            fs::create_dir_all(dest.parent().unwrap())?;
            std::io::copy(&mut file, &mut File::create_new(&dest)?)?;
            check_object(File::open(&dest)?, oid, *size)?;
        }
        paths.extend_from_slice(path.as_os_str().as_bytes());
        paths.push(0);
    }
    // The sandbox and host verifier can use their own installed LFS. Host Git
    // pins the executable separately each time it touches this private copy.
    git(checkout, &["config", "filter.lfs.process", "git-lfs filter-process"])?;
    git(checkout, &["config", "filter.lfs.required", "true"])?;
    let view = VerificationLfs {
        settings: vec![
            format!("filter.lfs.process={} filter-process", host::shell_path(&lfs)?),
            "filter.lfs.required=true".into(),
            format!("lfs.storage={}", storage.display()),
        ],
    };
    // checkout-index can leave a stat-clean pointer in place even with --force.
    // These are the inspected pointers in our private, as-yet unmounted checkout.
    for (path, _, _) in &pointers {
        fs::remove_file(checkout.join(path))?;
    }
    let args = ["checkout-index", "-z", "--stdin"];
    run(view.command(checkout, &args)?, &args, Some(&paths), SLOW).context("materializing verification LFS files")?;
    for (path, oid, size) in pointers {
        let staging::Entry::File(file) = staging::entry(&root, path)? else { bail!("missing LFS verification file") };
        check_object(file, &oid, size).with_context(|| format!("materializing LFS file {}", path.display()))?;
    }
    Ok(view)
}

fn check_object(mut file: File, oid: &str, size: u64) -> Result<()> {
    let mut hash = Sha256::new();
    let copied = std::io::copy(&mut file, &mut hash)?;
    ensure!(copied == size && format!("{:x}", hash.finalize()) == oid, "LFS object {oid} failed verification");
    Ok(())
}
