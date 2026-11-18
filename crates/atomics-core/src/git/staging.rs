//! Build index entries from confined descriptors. Git never walks a candidate
//! directory or resolves a gitfile to decide what to stage.
use super::*;
use rustix::fs::{AtFlags, FileType, Mode, OFlags, openat, readlinkat, statat};
use rustix::io::Errno;
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

pub(super) enum Entry {
    File(File),
    Link(Vec<u8>),
    Missing,
}

/// Hold each parent directory open until its child has been opened relative to it.
/// Neither stat/open races nor renamed parent components can redirect the file read.
pub(super) fn entry(root: &File, path: &Path) -> Result<Entry> {
    use std::path::Component;
    let mut dir = root.try_clone()?;
    let mut components = path.components().peekable();
    ensure!(components.peek().is_some(), "empty worktree path");
    while let Some(part) = components.next() {
        let Component::Normal(name) = part else { bail!("invalid relative worktree path") };
        ensure!(!name.as_bytes().eq_ignore_ascii_case(b".git"), "cannot read Git metadata as a worktree file");
        if components.peek().is_some() {
            let child = match openat(
                &dir,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => fd,
                Err(Errno::NOENT) => return Ok(Entry::Missing),
                Err(err) => return Err(err.into()),
            };
            ensure!(
                matches!(statat(&child, ".git", AtFlags::SYMLINK_NOFOLLOW), Err(Errno::NOENT)),
                "nested repository at {}",
                path.display()
            );
            dir = child.into();
            continue;
        }
        let stat = match statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return Ok(Entry::Missing),
            Err(err) => return Err(err.into()),
        };
        return match FileType::from_raw_mode(stat.st_mode) {
            FileType::Symlink => Ok(Entry::Link(readlinkat(&dir, name, Vec::new())?.into_bytes())),
            FileType::RegularFile => {
                let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
                let file = File::from(openat(&dir, name, flags, Mode::empty())?);
                ensure!(file.metadata()?.is_file(), "worktree file changed type");
                Ok(Entry::File(file))
            }
            _ => bail!("cannot stage directory, nested repository or special file at {}", path.display()),
        };
    }
    unreachable!()
}

pub(super) fn open_root(path: &Path) -> Result<File> {
    Ok(File::options().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(path)?)
}

struct IndexLock {
    path: PathBuf,
    file: File,
}

impl IndexLock {
    fn owns_path(&self) -> bool {
        self.path
            .symlink_metadata()
            .ok()
            .zip(self.file.metadata().ok())
            .is_some_and(|(a, b)| a.dev() == b.dev() && a.ino() == b.ino())
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        if self.owns_path() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub(super) fn stage(dir: &Path, paths: &[&str]) -> Result<String> {
    ensure_git_allowed(dir)?;
    let pins = match locate(dir)? {
        Some(pins) => pins,
        None => discover(dir)?,
    };
    ensure!(pins.git_dir.is_some(), "not a git repository");
    let dir_in_repo = dir.canonicalize()?;
    let prefix = dir_in_repo.strip_prefix(&pins.work_tree).context("path left its recorded worktree")?;
    let dir = &pins.work_tree;
    let folder = open_root(dir)?;
    let mut literal = Vec::new();
    let mut options = true;
    let mut all = false;
    for path in paths {
        match *path {
            "--" if options => options = false,
            "--all" | "-A" if options => all = true,
            "." => literal.push(prefix.to_owned()),
            p => {
                ensure!(
                    (!options || !p.starts_with('-'))
                        && !p.is_empty()
                        && Path::new(p).components().all(|c| matches!(c, std::path::Component::Normal(_))),
                    "invalid staging path"
                );
                literal.push(prefix.join(p));
            }
        }
    }
    if literal.is_empty() && !all {
        return Ok(String::new());
    }
    if literal.iter().any(|p| p.as_os_str().is_empty()) {
        literal.clear();
    }
    // Git writers take index.lock. Keep our working index in a host-private
    // directory: a task with direct filesystem access cannot change that copy.
    let live = pins.git_dir.as_ref().unwrap().join("index");
    let lock_path = live.with_added_extension("lock");
    let mut lock =
        IndexLock { file: File::create_new(&lock_path).context("could not lock the staging index")?, path: lock_path };
    let scratch = tempfile::tempdir()?;
    let snapshot = scratch.path().join("index");
    let open_index = || -> Result<Option<File>> {
        match File::options().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&live) {
            Ok(file) => {
                ensure!(file.metadata()?.is_file(), "invalid staging index");
                Ok(Some(file))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    };
    let identity = |file: &File| -> Result<_> {
        let m = file.metadata()?;
        Ok((m.dev(), m.ino(), m.len(), m.ctime(), m.ctime_nsec()))
    };
    let original = if let Some(mut file) = open_index()? {
        let original = identity(&file)?;
        let mut copy = File::create_new(&snapshot)?;
        std::io::copy(&mut file, &mut copy)?;
        // Git uses the index timestamp to detect same-size changes made during
        // its last write. A newer timestamp would hide these edits.
        copy.set_modified(file.metadata()?.modified()?)?;
        Some(original)
    } else {
        None
    };
    let indexed = |args: &[&str]| -> Result<_> {
        let mut cmd = command(dir, args)?;
        cmd.env("GIT_INDEX_FILE", &snapshot);
        Ok(cmd)
    };
    let index_args = ["ls-files", "--stage", "-z"];
    let index = output(indexed(&index_args)?, &index_args, None, quick())?;
    ensure!(index.status.success(), "cannot read the worktree index");
    ensure!(
        !index.stdout.split(|b| *b == 0).any(|entry| entry.starts_with(b"160000 ")),
        "automatic staging refuses an index containing gitlinks"
    );
    let mut args = vec![
        "--literal-pathspecs",
        "ls-files",
        "--deduplicate",
        "--others",
        "--modified",
        "--deleted",
        "--exclude-standard",
        "-z",
        "--",
    ];
    for path in &literal {
        args.push(path.to_str().context("non-UTF-8 staging path")?);
    }
    let names = output(indexed(&args)?, &args, None, quick())?;
    ensure!(names.status.success(), "cannot list files for staging");
    let raw_hash = ["hash-object", "-w", "--no-filters", "--stdin"];
    let filter_files = pins.common_dir.as_deref().is_some_and(registered_common);
    let mut changes = Vec::new();
    let mut deletion = None;
    for name in names.stdout.split(|b| *b == 0).filter(|name| !name.is_empty()) {
        let path = Path::new(std::ffi::OsStr::from_bytes(name));
        let (mode, object) = match entry(&folder, path)? {
            Entry::File(file) => {
                let mode = if file.metadata()?.mode() & 0o100 == 0 { "100644" } else { "100755" };
                let hash = ["hash-object", "-w", "--stdin"];
                let mut cmd = indexed(&hash)?;
                if filter_files {
                    // --path selects attributes; Git reads the bytes from the confined
                    // descriptor. Stream filters get the filename just as in git add.
                    cmd.arg("--path").arg(path);
                } else {
                    cmd.arg("--no-filters");
                }
                cmd.stdin(file);
                (mode, run(cmd, &hash, None, SLOW)?)
            }
            Entry::Link(target) => ("120000", git_input(dir, &raw_hash, &target)?),
            Entry::Missing => {
                if deletion.is_none() {
                    let format = git(dir, &["rev-parse", "--show-object-format"])?;
                    deletion = Some("0".repeat(match format.trim() {
                        "sha1" => 40,
                        "sha256" => 64,
                        other => bail!("unsupported Git object format {other}"),
                    }));
                }
                ("0", deletion.clone().unwrap())
            }
        };
        changes.extend_from_slice(format!("{mode} {}\t", object.trim()).as_bytes());
        changes.extend_from_slice(name);
        changes.push(0);
    }
    if !changes.is_empty() {
        let args = ["update-index", "-z", "--index-info"];
        run(indexed(&args)?, &args, Some(&changes), SLOW)?;
    }
    let args = ["write-tree"];
    let tree = run(indexed(&args)?, &args, None, SLOW)?;
    let args = ["ls-tree", "-r", "-z", tree.trim()];
    let entries = output(command(dir, &args)?, &args, None, SLOW)?;
    ensure!(entries.status.success(), "cannot validate staged tree");
    ensure!(
        !entries.stdout.split(|&b| b == 0).any(|entry| entry.starts_with(b"160000 ")),
        "automatic staging refuses a tree containing gitlinks"
    );
    ensure!(open_index()?.as_ref().map(identity).transpose()? == original, "worktree index changed during staging");
    ensure!(lock.owns_path(), "staging index lock changed");
    if !changes.is_empty() {
        let mut staged = File::open(snapshot)?;
        std::io::copy(&mut staged, &mut lock.file)?;
        lock.file.set_modified(staged.metadata()?.modified()?)?;
        ensure!(lock.owns_path(), "staging index lock changed");
        std::fs::rename(&lock.path, live)?;
    }
    Ok(tree)
}
