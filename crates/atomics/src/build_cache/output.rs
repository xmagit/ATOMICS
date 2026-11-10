//! Destructive output operations are anchored to directory descriptors, never to a
//! pathname that a guest could redirect between validation and deletion.
use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::fs::{File, Metadata};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use anyhow::{Result, ensure};

struct Directory(File);

impl Directory {
    fn root(path: &Path) -> io::Result<Self> {
        File::options().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(path).map(Self)
    }

    fn open(&self, name: &CStr) -> io::Result<Self> {
        // SAFETY: self owns a live directory fd and name is NUL-terminated. A
        // successful openat transfers a fresh fd to File exactly once.
        let fd = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(unsafe { File::from_raw_fd(fd) }))
    }

    fn stat(&self, name: &CStr) -> io::Result<libc::stat> {
        let mut stat = std::mem::MaybeUninit::uninit();
        // SAFETY: stat is writable and both fd and name remain valid for the call.
        if unsafe { libc::fstatat(self.0.as_raw_fd(), name.as_ptr(), stat.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) }
            != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fstatat initialized stat on success.
        Ok(unsafe { stat.assume_init() })
    }

    fn entries(&self) -> io::Result<Vec<CString>> {
        self.typed_entries().map(|entries| entries.into_iter().map(|(name, _)| name).collect())
    }

    fn typed_entries(&self) -> io::Result<Vec<(CString, u8)>> {
        // A new open description keeps readdir's offset separate from this fd.
        let fd = self.open(c".")?.0.into_raw_fd();
        // SAFETY: fd is a live, owned directory descriptor; fdopendir takes it on success.
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: fdopendir did not take ownership on failure.
            unsafe { libc::close(fd) };
            return Err(error);
        }
        struct Stream(*mut libc::DIR);
        impl Drop for Stream {
            fn drop(&mut self) {
                // SAFETY: this guard exclusively owns the live DIR and its fd.
                unsafe { libc::closedir(self.0) };
            }
        }
        let stream = Stream(stream);
        let mut entries = Vec::new();
        loop {
            // SAFETY: errno is thread-local, and stream owns a live DIR. readdir's
            // returned name is copied before the next call invalidates its buffer.
            unsafe {
                #[cfg(target_os = "linux")]
                {
                    *libc::__errno_location() = 0;
                }
                #[cfg(target_os = "macos")]
                {
                    *libc::__error() = 0;
                }
                let entry = libc::readdir(stream.0);
                if entry.is_null() {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() != Some(0) {
                        return Err(error);
                    }
                    break;
                }
                let name = CStr::from_ptr((*entry).d_name.as_ptr());
                if name != c"." && name != c".." {
                    entries.push((name.to_owned(), (*entry).d_type));
                }
            }
        }
        Ok(entries)
    }

    fn writable(&self) -> io::Result<()> {
        // SAFETY: the fd and NUL-terminated name are valid. Effective-ID checks
        // include ACLs and read-only mounts, unlike testing mode bits alone.
        if unsafe { libc::faccessat(self.0.as_raw_fd(), c".".as_ptr(), libc::W_OK | libc::X_OK, libc::AT_EACCESS) } == 0
        {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn unlink(&self, name: &CStr, directory: bool) -> io::Result<()> {
        // SAFETY: the directory fd and NUL-terminated name are valid. unlinkat
        // removes an entry relative to this fd and never follows the final link.
        let result = unsafe {
            libc::unlinkat(self.0.as_raw_fd(), name.as_ptr(), if directory { libc::AT_REMOVEDIR } else { 0 })
        };
        if result == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    }
}

#[derive(Clone, Copy)]
struct Boundary {
    device: u64,
    #[cfg(target_os = "linux")]
    mount: u64,
}

impl Boundary {
    fn of(root: &Directory) -> Result<Self> {
        Ok(Self {
            device: root.0.metadata()?.dev(),
            #[cfg(target_os = "linux")]
            mount: mount_id(root, c"")?,
        })
    }

    fn check(&self, parent: &Directory, name: &CStr, stat: &libc::stat) -> Result<()> {
        #[allow(clippy::unnecessary_cast)] // st_dev differs across Unix targets.
        let device = stat.st_dev as u64;
        ensure!(device == self.device, "output crosses a filesystem device boundary");
        #[cfg(target_os = "linux")]
        ensure!(mount_id(parent, name)? == self.mount, "output crosses a mount boundary");
        #[cfg(not(target_os = "linux"))]
        let _ = (parent, name);
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn mount_id(parent: &Directory, name: &CStr) -> Result<u64> {
    let mut stat = std::mem::MaybeUninit::<libc::statx>::uninit();
    // SAFETY: stat is writable; fd/name are valid. AT_EMPTY_PATH supports checking
    // the opened directory itself. NOFOLLOW checks links rather than their targets.
    let result = unsafe {
        libc::statx(
            parent.0.as_raw_fd(),
            name.as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            libc::STATX_MNT_ID,
            stat.as_mut_ptr(),
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: statx initialized the structure on success; the mask is checked below.
    let stat = unsafe { stat.assume_init() };
    require_mount_id(stat.stx_mask, stat.stx_mnt_id)
}

#[cfg(target_os = "linux")]
fn require_mount_id(mask: u32, id: u64) -> Result<u64> {
    ensure!(
        mask & libc::STATX_MNT_ID != 0,
        "cleanup requires STATX_MNT_ID (Linux 5.8 or newer); output mount identity is unavailable"
    );
    Ok(id)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    fn metadata(meta: &Metadata) -> Self {
        Self { device: meta.dev(), inode: meta.ino() }
    }
    fn stat(stat: &libc::stat) -> Self {
        #[allow(clippy::unnecessary_cast)]
        Self { device: stat.st_dev as u64, inode: stat.st_ino as u64 }
    }
}

fn same_directory(meta: &Metadata, stat: &libc::stat) -> bool {
    #[allow(clippy::unnecessary_cast)]
    {
        meta.dev() == stat.st_dev as u64 && meta.ino() == stat.st_ino as u64
    }
}

fn directory(stat: &libc::stat) -> bool {
    stat.st_mode & libc::S_IFMT == libc::S_IFDIR
}

fn checked_entry(parent: &Directory, name: &CStr, boundary: Boundary) -> Result<libc::stat> {
    ensure!(!name.to_bytes().eq_ignore_ascii_case(b".git"), "output contains a nested repository");
    // Ignore-rule provenance is checked by the caller for the output directory.
    // Git never reads nested .gitignore files below an already ignored output.
    let stat = parent.stat(name)?;
    boundary.check(parent, name, &stat)?;
    let kind = stat.st_mode & libc::S_IFMT;
    ensure!(matches!(kind, libc::S_IFDIR | libc::S_IFREG | libc::S_IFLNK), "output contains a special file");
    Ok(stat)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WalkMode {
    Measure,
    Preflight,
    Remove,
}

struct Walk<'a> {
    boundary: Boundary,
    expected: Identity,
    mode: WalkMode,
    changed: usize,
    seen: &'a mut HashSet<Identity>,
    added: Vec<Identity>,
}

fn walk_tree(
    parent: &Directory,
    name: &CStr,
    depth: usize,
    state: &mut Walk<'_>,
    unlink: &mut impl FnMut(&Directory, &CStr, bool) -> io::Result<()>,
) -> Result<u64> {
    ensure!(depth < 128, "output directory nesting is too deep");
    let stat = checked_entry(parent, name, state.boundary)?;
    ensure!(
        depth != 0 || (directory(&stat) && Identity::stat(&stat) == state.expected),
        "output directory identity changed"
    );
    if !directory(&stat) {
        let bytes = if stat.st_nlink > 1 {
            let identity = Identity::stat(&stat);
            if state.seen.insert(identity) {
                state.added.push(identity);
                (stat.st_blocks as u64).saturating_mul(512)
            } else {
                0
            }
        } else {
            (stat.st_blocks as u64).saturating_mul(512)
        };
        if state.mode == WalkMode::Remove {
            unlink(parent, name, false)?;
            state.changed += 1;
        }
        return Ok(bytes);
    }
    let child = parent.open(name)?;
    ensure!(same_directory(&child.0.metadata()?, &stat), "output changed while opening a directory");
    state.boundary.check(&child, c"", &child.stat(c".")?)?;
    if state.mode != WalkMode::Measure {
        child.writable().map_err(|error| anyhow::anyhow!("output directory lacks write/search access: {error}"))?;
    }
    let mut bytes = (stat.st_blocks as u64).saturating_mul(512);
    for entry in child.entries()? {
        bytes = bytes.saturating_add(walk_tree(&child, &entry, depth + 1, state, unlink)?);
    }
    if state.mode == WalkMode::Remove {
        ensure!(same_directory(&child.0.metadata()?, &parent.stat(name)?), "output changed during removal");
        state.boundary.check(parent, name, &parent.stat(name)?)?;
        unlink(parent, name, true)?;
        state.changed += 1;
    }
    Ok(bytes)
}

#[cfg(test)]
fn walk(parent: &Directory, name: &CStr, boundary: Boundary, depth: usize, remove: bool) -> Result<u64> {
    let mut seen = HashSet::new();
    let mut state = Walk {
        boundary,
        expected: Identity::stat(&parent.stat(name)?),
        mode: if remove { WalkMode::Remove } else { WalkMode::Preflight },
        changed: 0,
        seen: &mut seen,
        added: Vec::new(),
    };
    walk_tree(parent, name, depth, &mut state, &mut Directory::unlink)
}

#[derive(Debug)]
pub(super) struct PartialRemoval {
    pub path: std::path::PathBuf,
    pub left: Option<u64>,
    error: anyhow::Error,
}

impl std::fmt::Display for PartialRemoval {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let remaining =
            self.left.map_or_else(|| "remaining size unavailable".into(), |bytes| format!("{bytes} bytes left"));
        write!(fmt, "partially removed {}: {:#}; {remaining}", self.path.display(), self.error)
    }
}
impl std::error::Error for PartialRemoval {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

pub(super) struct Output {
    root: Directory,
    parent: Directory,
    parents: Vec<CString>,
    name: CString,
    boundary: Boundary,
    identity: Identity,
    directory: Directory,
    path: std::path::PathBuf,
}

impl Output {
    pub(super) fn open(root: &Path, folder: &str) -> Result<Self> {
        ensure!(super::relative(folder), "invalid output path");
        let path = root.join(folder);
        let expected = root.metadata()?;
        let root = Directory::root(root)?;
        let current = root.0.metadata()?;
        ensure!(
            expected.dev() == current.dev() && expected.ino() == current.ino(),
            "worktree changed while opening it"
        );
        let boundary = Boundary::of(&root)?;
        let mut components: Vec<_> =
            Path::new(folder).iter().map(|part| CString::new(part.as_bytes())).collect::<Result<_, _>>()?;
        let name = components.pop().ok_or_else(|| anyhow::anyhow!("empty output path"))?;
        let mut parent = Directory(root.0.try_clone()?);
        for part in &components {
            ensure!(!part.to_bytes().eq_ignore_ascii_case(b".git"), "output is inside repository metadata");
            ensure!(parent.entries()?.contains(part), "output component name changed");
            let stat = parent.stat(part)?;
            boundary.check(&parent, part, &stat)?;
            parent = parent.open(part)?;
            ensure!(same_directory(&parent.0.metadata()?, &stat), "output parent changed");
            match parent.stat(c".git") {
                Ok(_) => anyhow::bail!("output is inside a nested repository"),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        ensure!(parent.entries()?.contains(&name), "output name changed");
        let stat = parent.stat(&name)?;
        ensure!(directory(&stat), "output is no longer a directory");
        let directory = parent.open(&name)?;
        let identity = Identity::metadata(&directory.0.metadata()?);
        ensure!(identity == Identity::stat(&stat), "output directory changed while opening");
        Ok(Self { root, parent, parents: components, name, boundary, identity, directory, path })
    }

    pub(super) fn inspect(&self) -> Result<u64> {
        self.parent.writable().map_err(|error| anyhow::anyhow!("output parent lacks write/search access: {error}"))?;
        let mut seen = HashSet::new();
        let mut state = Walk {
            boundary: self.boundary,
            expected: self.identity,
            mode: WalkMode::Preflight,
            changed: 0,
            seen: &mut seen,
            added: Vec::new(),
        };
        walk_tree(&self.parent, &self.name, 0, &mut state, &mut Directory::unlink)
    }

    pub(super) fn measure(&self, seen: &mut HashSet<Identity>) -> Result<u64> {
        ensure!(Identity::stat(&self.parent.stat(&self.name)?) == self.identity, "output directory identity changed");
        self.measure_held(seen)
    }

    fn measure_held(&self, seen: &mut HashSet<Identity>) -> Result<u64> {
        let mut state = Walk {
            boundary: self.boundary,
            expected: self.identity,
            mode: WalkMode::Measure,
            changed: 0,
            seen,
            added: Vec::new(),
        };
        let measured = walk_tree(&self.directory, c".", 0, &mut state, &mut Directory::unlink);
        if measured.is_err() {
            for identity in state.added {
                state.seen.remove(&identity);
            }
        }
        measured
    }

    #[cfg(test)]
    fn remove(self) -> Result<()> {
        self.remove_if(|| Ok(()))
    }

    pub(super) fn remove_if(self, allowed: impl FnOnce() -> Result<()>) -> Result<()> {
        self.remove_using(allowed, Directory::unlink)
    }

    fn remove_using(
        self,
        allowed: impl FnOnce() -> Result<()>,
        mut unlink: impl FnMut(&Directory, &CStr, bool) -> io::Result<()>,
    ) -> Result<()> {
        // Re-open the route from the held root before removing anything. Even a
        // later swap cannot redirect unlinkat through the anchored parent fd.
        let mut current = Directory(self.root.0.try_clone()?);
        for part in &self.parents {
            current = current.open(part)?;
        }
        let meta = current.0.metadata()?;
        let expected = self.parent.0.metadata()?;
        ensure!(meta.dev() == expected.dev() && meta.ino() == expected.ino(), "output parent moved");
        self.inspect()?;
        allowed()?;
        let mut seen = HashSet::new();
        let mut state = Walk {
            boundary: self.boundary,
            expected: self.identity,
            mode: WalkMode::Remove,
            changed: 0,
            seen: &mut seen,
            added: Vec::new(),
        };
        match walk_tree(&self.parent, &self.name, 0, &mut state, &mut unlink) {
            Err(error) if state.changed > 0 => Err(PartialRemoval {
                path: self.path.clone(),
                left: self.measure_held(&mut HashSet::new()).ok(),
                error,
            }
            .into()),
            result => result.map(|_| ()),
        }
    }
}

impl Directory {
    fn read_link(&self, name: &CStr) -> io::Result<std::path::PathBuf> {
        use std::os::unix::ffi::OsStringExt;
        let mut bytes = vec![0; 256];
        loop {
            // SAFETY: bytes is writable for its length; fd and name are valid.
            let count =
                unsafe { libc::readlinkat(self.0.as_raw_fd(), name.as_ptr(), bytes.as_mut_ptr().cast(), bytes.len()) };
            if count < 0 {
                return Err(io::Error::last_os_error());
            }
            if (count as usize) < bytes.len() {
                bytes.truncate(count as usize);
                return Ok(std::ffi::OsString::from_vec(bytes).into());
            }
            if bytes.len() >= 65536 {
                return Err(io::Error::other("cache link target is too long"));
            }
            bytes.resize(bytes.len() * 2, 0);
        }
    }
}

impl Directory {
    fn unlink_link(&self, name: &CStr, expected: &libc::stat, after_open: impl FnOnce()) -> Result<()> {
        // Pin the link itself, never its target, through the same parent fd used
        // by unlinkat. Holding it also prevents inode reuse during validation.
        #[cfg(target_os = "linux")]
        let flags = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        #[cfg(target_os = "macos")]
        let flags = libc::O_RDONLY | libc::O_SYMLINK | libc::O_CLOEXEC;
        // SAFETY: fd/name are valid. File owns the new fd exactly once.
        let fd = unsafe { libc::openat(self.0.as_raw_fd(), name.as_ptr(), flags) };
        ensure!(fd >= 0, "cannot pin cache link: {}", io::Error::last_os_error());
        let link = unsafe { File::from_raw_fd(fd) };
        let pinned = link.metadata()?;
        ensure!(
            pinned.file_type().is_symlink() && same_directory(&pinned, expected),
            "cache link changed while opening"
        );
        after_open();
        let current = self.stat(name)?;
        ensure!(
            current.st_mode & libc::S_IFMT == libc::S_IFLNK && same_directory(&pinned, &current),
            "cache link changed during repair"
        );
        // Unix has no conditional unlink-by-inode. Keep this descriptor-relative
        // check adjacent to unlink; concurrent outside writers remain documented.
        self.unlink(name, false)?;
        Ok(())
    }
}

/// Inspect only configured and allowlisted cache routes. Do not enumerate build
/// output, dependency trees or unrelated worktree folders during a session launch.
pub(super) fn repair_links(
    root: &Path,
    locations: &[String],
    protected: &[std::path::PathBuf],
    forbidden: impl Fn(&Path) -> bool,
) -> Result<Vec<std::path::PathBuf>> {
    let directory = Directory::root(root)?;
    let boundary = Boundary::of(&directory)?;
    let mut removed = Vec::new();
    for location in locations.iter().filter(|path| super::relative(path)) {
        let result = (|| -> Result<()> {
            let mut dir = Directory(directory.0.try_clone()?);
            let mut path = root.to_path_buf();
            let parts: Vec<_> = Path::new(location).iter().collect();
            for (index, part) in parts.iter().enumerate() {
                ensure!(!part.as_bytes().eq_ignore_ascii_case(b".git"), "cache path enters repository metadata");
                let child_path = path.join(part);
                if protected.iter().any(|nested| child_path.starts_with(nested)) {
                    break;
                }
                let name = CString::new(part.as_bytes())?;
                let stat = match dir.stat(&name) {
                    Ok(stat) => stat,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                    Err(error) => return Err(error.into()),
                };
                boundary.check(&dir, &name, &stat)?;
                if stat.st_mode & libc::S_IFMT == libc::S_IFLNK {
                    let target = dir.read_link(&name)?;
                    let target = if target.is_absolute() { target } else { path.join(target) };
                    if forbidden(&target) {
                        dir.unlink_link(&name, &stat, || {})?;
                        eprintln!(
                            "ATOMICS: removed forbidden cache link {} (store contents kept)",
                            child_path.display()
                        );
                        removed.push(child_path);
                    }
                    break; // Never follow any link, including an ordinary cache link.
                }
                if !self::directory(&stat) || index + 1 == parts.len() {
                    break;
                }
                let child = dir.open(&name)?;
                ensure!(same_directory(&child.0.metadata()?, &stat), "cache directory changed during repair");
                boundary.check(&child, c"", &child.stat(c".")?)?;
                match child.stat(c".git") {
                    Ok(_) => break,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                dir = child;
                path = child_path;
            }
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("ATOMICS: cache link scan skipped {}: {error:#}", root.join(location).display());
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removal_refuses_a_swapped_intermediate_component_and_never_follows_a_leaf_link() {
        let root = crate::test_support::tempdir().unwrap();
        let outside = crate::test_support::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("web/dist")).unwrap();
        std::fs::write(root.path().join("web/dist/artifact"), "compiled").unwrap();
        std::fs::create_dir(outside.path().join("dist")).unwrap();
        std::fs::write(outside.path().join("dist/source"), "keep").unwrap();
        let output = Output::open(root.path(), "web/dist").unwrap();
        output.inspect().unwrap();
        std::fs::rename(root.path().join("web"), root.path().join("old-web")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("web")).unwrap();
        assert!(output.remove().is_err());
        assert!(root.path().join("old-web/dist/artifact").is_file());
        assert!(outside.path().join("dist/source").is_file());
        std::os::unix::fs::symlink(outside.path(), root.path().join("old-web/dist/link")).unwrap();
        Output::open(root.path(), "old-web/dist").unwrap().remove().unwrap();
        assert!(outside.path().join("dist/source").is_file());
        assert!(!root.path().join("old-web/dist").exists());
    }

    #[test]
    fn a_top_level_output_swapped_for_a_link_is_kept() {
        let root = crate::test_support::tempdir().unwrap();
        let outside = crate::test_support::tempdir().unwrap();
        std::fs::create_dir(root.path().join("target")).unwrap();
        std::fs::write(root.path().join("target/artifact"), "compiled").unwrap();
        std::fs::write(outside.path().join("source"), "keep").unwrap();
        let output = Output::open(root.path(), "target").unwrap();
        output.inspect().unwrap();
        std::fs::rename(root.path().join("target"), root.path().join("old-target")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("target")).unwrap();
        assert!(output.remove().is_err());
        assert!(outside.path().join("source").is_file());
        assert!(root.path().join("old-target/artifact").is_file());
        assert!(root.path().join("target").symlink_metadata().unwrap().file_type().is_symlink());
    }

    #[test]
    fn replacement_directory_after_git_checks_is_never_removed() {
        let root = crate::test_support::tempdir().unwrap();
        std::fs::create_dir(root.path().join("target")).unwrap();
        std::fs::write(root.path().join("target/artifact"), "compiled").unwrap();
        let output = Output::open(root.path(), "target").unwrap();
        let result = output.remove_if(|| {
            std::fs::rename(root.path().join("target"), root.path().join("old-target"))?;
            std::fs::create_dir(root.path().join("target"))?;
            std::fs::write(root.path().join("target/new.rs"), "untracked source")?;
            Ok(())
        });
        assert!(result.unwrap_err().to_string().contains("identity changed"));
        assert_eq!(std::fs::read_to_string(root.path().join("target/new.rs")).unwrap(), "untracked source");
        assert!(root.path().join("old-target/artifact").is_file());
    }

    #[test]
    fn link_repair_pins_and_rechecks_the_link_through_its_parent_descriptor() {
        let root = crate::test_support::tempdir().unwrap();
        let outside = crate::test_support::tempdir().unwrap();
        std::fs::write(outside.path().join("source"), "keep").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("cache")).unwrap();
        let parent = Directory::root(root.path()).unwrap();
        let stat = parent.stat(c"cache").unwrap();
        let result = parent.unlink_link(c"cache", &stat, || {
            std::fs::rename(root.path().join("cache"), root.path().join("old-link")).unwrap();
            std::fs::write(root.path().join("cache"), "new source").unwrap();
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(root.path().join("cache")).unwrap(), "new source");
        assert!(outside.path().join("source").is_file());
        let stat = parent.stat(c"old-link").unwrap();
        parent.unlink_link(c"old-link", &stat, || {}).unwrap();
        assert!(!root.path().join("old-link").exists());
        assert!(outside.path().join("source").is_file());
    }

    #[test]
    fn permission_preflight_preserves_the_entire_output() {
        use std::os::unix::fs::PermissionsExt;
        // Run the same regression as an unprivileged child when the test runner
        // is root, whose effective-ID access checks correctly bypass mode bits.
        if let Some(root) = std::env::var_os("ATOMICS_PERMISSION_PROBE") {
            let root = Path::new(&root);
            let error = Output::open(root, "target").unwrap().remove().unwrap_err();
            assert!(format!("{error:#}").contains("write/search access"));
            assert!(root.join("target/artifact").is_file());
            assert!(root.join("target/locked/artifact").is_file());
            return;
        }
        let root = crate::test_support::tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        std::fs::create_dir_all(root.path().join("target/locked")).unwrap();
        std::fs::set_permissions(root.path().join("target"), std::fs::Permissions::from_mode(0o777)).unwrap();
        std::fs::write(root.path().join("target/artifact"), "compiled").unwrap();
        std::fs::write(root.path().join("target/locked/artifact"), "compiled").unwrap();
        std::fs::set_permissions(root.path().join("target/locked"), std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = if unsafe { libc::geteuid() } == 0 {
            use std::os::unix::process::CommandExt;
            let probe = root.path().join("permission-probe");
            crate::test_support::copy_program(std::env::current_exe().unwrap(), &probe);
            let mut child = std::process::Command::new(probe);
            child
                .args([
                    "--exact",
                    "build_cache::output::tests::permission_preflight_preserves_the_entire_output",
                    "--nocapture",
                ])
                .env("ATOMICS_PERMISSION_PROBE", root.path())
                .uid(65534)
                .gid(65534);
            let output = child.output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            Ok(())
        } else {
            Output::open(root.path(), "target").unwrap().remove()
        };
        std::fs::set_permissions(root.path().join("target/locked"), std::fs::Permissions::from_mode(0o755)).unwrap();
        if unsafe { libc::geteuid() } != 0 {
            assert!(format!("{:#}", result.unwrap_err()).contains("write/search access"));
        }
        assert!(root.path().join("target/artifact").is_file());
        assert!(root.path().join("target/locked/artifact").is_file());
    }

    #[test]
    fn an_unexpected_unlink_failure_reports_partial_removal_and_remaining_bytes() {
        let root = crate::test_support::tempdir().unwrap();
        std::fs::create_dir(root.path().join("target")).unwrap();
        for file in ["one", "two"] {
            std::fs::write(root.path().join("target").join(file), vec![1; 8192]).unwrap();
        }
        let output = Output::open(root.path(), "target").unwrap();
        let mut calls = 0;
        let error = output
            .remove_using(
                || Ok(()),
                |parent, name, directory| {
                    calls += 1;
                    if calls == 2 {
                        Err(io::Error::from(io::ErrorKind::PermissionDenied))
                    } else {
                        parent.unlink(name, directory)
                    }
                },
            )
            .unwrap_err();
        let partial = error.downcast_ref::<PartialRemoval>().unwrap();
        assert_eq!(partial.path, root.path().join("target"));
        assert_eq!(std::fs::read_dir(&partial.path).unwrap().count(), 1);
        let left = Output::open(root.path(), "target").unwrap().measure(&mut HashSet::new()).unwrap();
        assert_eq!(partial.left, Some(left));
        let notice = super::super::partial_removal(&error).unwrap();
        assert!(notice.contains("partially removed") && notice.contains(&format!("{left} bytes left")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_statx_mount_identity_refuses_cleanup_with_a_supported_kernel_hint() {
        assert!(require_mount_id(libc::STATX_BASIC_STATS, 123).unwrap_err().to_string().contains("Linux 5.8"));
        assert_eq!(require_mount_id(libc::STATX_MNT_ID, 123).unwrap(), 123);
    }

    #[test]
    fn device_boundaries_abort_before_any_output_is_removed() {
        let root = crate::test_support::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("target/nested")).unwrap();
        std::fs::write(root.path().join("target/nested/artifact"), "compiled").unwrap();
        let output = Output::open(root.path(), "target").unwrap();
        let mut foreign = output.boundary;
        foreign.device = foreign.device.wrapping_add(1);
        assert!(walk(&output.parent, &output.name, foreign, 0, true).is_err());
        let target = output.parent.open(&output.name).unwrap();
        // The same guard is applied to descendants as well as the top directory.
        assert!(walk(&target, c"nested", foreign, 1, true).is_err());
        assert!(root.path().join("target/nested/artifact").is_file());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_foreign_filesystem_directory_is_never_emptied() {
        let root = crate::test_support::tempdir().unwrap();
        let foreign = crate::test_support::tempdir_in("/dev/shm").unwrap();
        let root_dir = Directory::root(root.path()).unwrap();
        let boundary = Boundary::of(&root_dir).unwrap();
        let foreign_dir = Directory::root(foreign.path()).unwrap();
        assert_ne!(root_dir.0.metadata().unwrap().dev(), foreign_dir.0.metadata().unwrap().dev());
        std::fs::create_dir(foreign.path().join("target")).unwrap();
        std::fs::write(foreign.path().join("target/source"), "keep").unwrap();
        assert!(walk(&foreign_dir, c"target", boundary, 0, true).is_err());
        assert_eq!(std::fs::read_to_string(foreign.path().join("target/source")).unwrap(), "keep");
    }
}
