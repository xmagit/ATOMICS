//! Destructive output operations are anchored to directory descriptors, never to a
//! pathname that a guest could redirect between validation and deletion.
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
    ensure!(stat.stx_mask & libc::STATX_MNT_ID != 0, "cannot verify output mount identity");
    Ok(stat.stx_mnt_id)
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
    // A tracked .gitignore already disqualifies the output via ls-files. Any other
    // .gitignore must not be allowed to hide uncommitted work from status.
    ensure!(!name.to_bytes().eq_ignore_ascii_case(b".gitignore"), "output contains an untracked ignore file");
    let stat = parent.stat(name)?;
    boundary.check(parent, name, &stat)?;
    let kind = stat.st_mode & libc::S_IFMT;
    ensure!(matches!(kind, libc::S_IFDIR | libc::S_IFREG | libc::S_IFLNK), "output contains a special file");
    Ok(stat)
}

fn walk(parent: &Directory, name: &CStr, boundary: Boundary, depth: usize, remove: bool) -> Result<u64> {
    ensure!(depth < 128, "output directory nesting is too deep");
    let stat = checked_entry(parent, name, boundary)?;
    ensure!(depth != 0 || directory(&stat), "output is no longer a directory");
    if !directory(&stat) {
        if remove {
            parent.unlink(name, false)?;
        }
        return Ok((stat.st_blocks as u64).saturating_mul(512));
    }
    let child = parent.open(name)?;
    ensure!(same_directory(&child.0.metadata()?, &stat), "output changed while opening a directory");
    boundary.check(&child, c"", &child.stat(c".")?)?;
    let mut bytes = (stat.st_blocks as u64).saturating_mul(512);
    for entry in child.entries()? {
        bytes = bytes.saturating_add(walk(&child, &entry, boundary, depth + 1, remove)?);
    }
    if remove {
        ensure!(same_directory(&child.0.metadata()?, &parent.stat(name)?), "output changed during removal");
        boundary.check(parent, name, &parent.stat(name)?)?;
        parent.unlink(name, true)?;
    }
    Ok(bytes)
}

pub(super) struct Output {
    root: Directory,
    parent: Directory,
    parents: Vec<CString>,
    name: CString,
    boundary: Boundary,
}

impl Output {
    pub(super) fn open(root: &Path, folder: &str) -> Result<Self> {
        ensure!(super::relative(folder), "invalid output path");
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
        ensure!(directory(&parent.stat(&name)?), "output is no longer a directory");
        Ok(Self { root, parent, parents: components, name, boundary })
    }

    pub(super) fn inspect(&self) -> Result<u64> {
        walk(&self.parent, &self.name, self.boundary, 0, false)
    }

    #[cfg(test)]
    fn remove(self) -> Result<()> {
        self.remove_if(|| Ok(()))
    }

    pub(super) fn remove_if(self, allowed: impl FnOnce() -> Result<()>) -> Result<()> {
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
        walk(&self.parent, &self.name, self.boundary, 0, true)?;
        Ok(())
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

/// Remove only links into forbidden stores, leaving all store contents and other
/// links alone. Scan through held directory fds without entering nested checkouts
/// or mount points. This also finds links left by removed cache configuration.
pub(super) fn repair_links(
    root: &Path,
    protected: &[std::path::PathBuf],
    forbidden: impl Fn(&Path) -> bool,
) -> Result<Vec<std::path::PathBuf>> {
    fn visit(
        dir: &Directory,
        path: &Path,
        boundary: Boundary,
        forbidden: &impl Fn(&Path) -> bool,
        protected: &[std::path::PathBuf],
        removed: &mut Vec<std::path::PathBuf>,
        depth: usize,
    ) -> Result<()> {
        ensure!(depth < 128, "cache link scan is too deeply nested");
        for (name, kind) in dir.typed_entries()? {
            // Cache repair needs links and directories, not metadata for every
            // artifact in a large target tree. Unknown d_type still gets checked.
            if kind == libc::DT_REG || name.to_bytes().eq_ignore_ascii_case(b".git") {
                continue;
            }
            let child_path = path.join(std::ffi::OsStr::from_bytes(name.to_bytes()));
            if protected.iter().any(|nested| child_path.starts_with(nested)) {
                continue;
            }
            let result = (|| -> Result<()> {
                let stat = dir.stat(&name)?;
                if boundary.check(dir, &name, &stat).is_err() {
                    return Ok(());
                }
                if stat.st_mode & libc::S_IFMT == libc::S_IFLNK {
                    let target = dir.read_link(&name)?;
                    let target = if target.is_absolute() { target } else { path.join(target) };
                    if forbidden(&target) {
                        let current = dir.stat(&name)?;
                        ensure!(
                            current.st_ino == stat.st_ino
                                && current.st_dev == stat.st_dev
                                && current.st_mode == stat.st_mode,
                            "cache link changed during repair"
                        );
                        dir.unlink(&name, false)?;
                        eprintln!(
                            "ATOMICS: removed forbidden cache link {} (store contents kept)",
                            child_path.display()
                        );
                        removed.push(child_path.clone());
                    }
                } else if directory(&stat) {
                    let child = dir.open(&name)?;
                    ensure!(same_directory(&child.0.metadata()?, &stat), "cache directory changed during repair");
                    match child.stat(c".git") {
                        Ok(_) => return Ok(()),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                    visit(&child, &child_path, boundary, forbidden, protected, removed, depth + 1)?;
                }
                Ok(())
            })();
            if let Err(error) = result {
                eprintln!("ATOMICS: cache link scan skipped {}: {error:#}", child_path.display());
            }
        }
        Ok(())
    }
    let directory = Directory::root(root)?;
    let mut removed = Vec::new();
    visit(&directory, root, Boundary::of(&directory)?, &forbidden, protected, &mut removed, 0)?;
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
