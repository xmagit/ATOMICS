//! Native storage for attachments.
use super::*;
use base64::Engine;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct Inbox {
    home: PathBuf,
    pub config: Config,
    pending: HashMap<u64, Transfer>,
}
struct Transfer {
    id: String,
    dir: tempfile::TempDir,
    files: Vec<FileInfo>,
    writers: Vec<(File, u64, Sha256)>,
    exchange: bool,
    bytes: u64,
    started: Instant,
    touched: Instant,
}

fn error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// Walk from an already-open directory. No intermediate component can be a symlink.
fn child(parent: &File, name: &str, directory: bool) -> Result<File, String> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        return Err("Invalid attachment component".into());
    }
    let name = std::ffi::CString::new(name).map_err(error)?;
    let flags = libc::O_RDONLY
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: parent and name remain valid for openat. A successful fd is owned once.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(error(std::io::Error::last_os_error()));
    }
    let file = unsafe { File::from_raw_fd(fd) };
    if !directory && !file.metadata().map_err(error)?.is_file() {
        return Err("Regular files only".into());
    }
    Ok(file)
}

/// A user's local path is used only on their client. The final component cannot be a link/device.
pub fn read_local(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let file =
        OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path).map_err(error)?;
    let meta = file.metadata().map_err(error)?;
    if !meta.is_file() {
        return Err("Regular files only (no folders, links or devices)".into());
    }
    if meta.len() > limit {
        return Err("File exceeds the byte limit".into());
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes).map_err(error)?;
    if bytes.len() as u64 > limit {
        return Err("File grew past the byte limit".into());
    }
    Ok(bytes)
}

fn private_root(home: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(home).map_err(error)?;
    let home = home.canonicalize().map_err(error)?;
    let root = home.join("attachments");
    match std::fs::DirBuilder::new().mode(0o700).create(&root) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(error(e)),
    }
    let directory = child(&File::open(home).map_err(error)?, "attachments", true)?;
    use std::os::unix::fs::PermissionsExt;
    directory.set_permissions(std::fs::Permissions::from_mode(0o700)).map_err(error)?;
    Ok(root)
}

impl Inbox {
    pub fn new(home: &Path, config: Config) -> Self {
        Self { home: home.into(), config, pending: HashMap::new() }
    }
    pub fn expire(&mut self) {
        let now = Instant::now();
        let expired: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, t)| {
                now.duration_since(t.touched).as_secs() >= self.config.idle_secs
                    || now.duration_since(t.started).as_secs() >= self.config.timeout_secs
            })
            .map(|(owner, _)| *owner)
            .collect();
        for owner in expired {
            self.disconnect(owner);
        }
    }
    pub fn request(&mut self, owner: u64, request: Request) -> Result<Reply, String> {
        self.expire();
        if matches!(request, Request::Limits) {
            return Ok(Reply {
                limits: Some(self.config.clone()),
                id: String::new(),
                error: None,
                complete: false,
                files: vec![],
            });
        }
        if let Request::Begin { mut files, exchange } = request {
            if self.pending.contains_key(&owner) {
                return Err("An attachment is already uploading".into());
            }
            let bytes = validate(&files, &self.config)?;
            let reserved: u64 = self.pending.values().map(|t| t.bytes).sum();
            if bytes > self.config.max_staging_bytes.saturating_sub(reserved) {
                return Err("Host attachment staging is full".into());
            }
            if exchange && self.config.exchange.is_none() {
                return Err("No exchange share configured on this host".into());
            }
            let root = private_root(&self.home)?;
            let dir = tempfile::Builder::new().prefix("drop-").rand_bytes(16).tempdir_in(root).map_err(error)?;
            let id = dir.path().file_name().unwrap().to_string_lossy().into_owned();
            let mut writers = Vec::new();
            for (index, file) in files.iter_mut().enumerate() {
                file.name = safe_name(&file.name);
                // Each file has its own directory, so duplicate names cannot collide.
                let folder = dir.path().join(index.to_string());
                std::fs::DirBuilder::new().mode(0o700).create(&folder).map_err(error)?;
                let writer = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(folder.join(&file.name))
                    .map_err(error)?;
                writers.push((writer, 0, Sha256::new()));
            }
            let now = Instant::now();
            self.pending.insert(
                owner,
                Transfer { id: id.clone(), dir, files, writers, exchange, bytes, started: now, touched: now },
            );
            return Ok(Reply { limits: None, id, error: None, complete: false, files: vec![] });
        }
        let id = match &request {
            Request::Chunk { id, .. } | Request::Commit { id } | Request::Cancel { id } => id,
            _ => unreachable!(),
        };
        if !self.pending.get(&owner).is_some_and(|t| t.id == *id) {
            return Err("Attachment is absent or belongs to another connection".into());
        }
        let result = (|| match request {
            Request::Chunk { id, file, offset, data } => {
                let transfer = self.pending.get_mut(&owner).unwrap();
                if transfer.exchange {
                    return Err("Exchange transfers do not accept chunks".into());
                }
                if data.len() > CHUNK_BYTES.div_ceil(3) * 4 {
                    return Err("Attachment chunk is too large".into());
                }
                let bytes = base64::engine::general_purpose::STANDARD.decode(data).map_err(error)?;
                transfer.write(file, offset, &bytes)?;
                Ok(Reply { limits: None, id, error: None, complete: false, files: vec![] })
            }
            Request::Cancel { id } => {
                self.disconnect(owner);
                Ok(Reply { limits: None, id, error: None, complete: true, files: vec![] })
            }
            Request::Commit { id } => {
                let mut transfer = self.pending.remove(&owner).unwrap();
                let result = transfer.finish(self.config.exchange.as_deref());
                self.remove_exchange(&transfer);
                result.map(|files| Reply { limits: None, id, error: None, complete: true, files })
            }
            _ => unreachable!(),
        })();
        if result.is_err() {
            self.disconnect(owner);
        }
        result
    }
    pub fn disconnect(&mut self, owner: u64) {
        if let Some(transfer) = self.pending.remove(&owner) {
            self.remove_exchange(&transfer);
        }
    }
    fn remove_exchange(&self, transfer: &Transfer) {
        if !transfer.exchange {
            return;
        }
        let Some(root) = &self.config.exchange else {
            return;
        };
        let Ok(root) = File::open(crate::paths::expand(root)) else {
            return;
        };
        let Ok(root) = child(&root, ".atomics-attachments", true) else {
            return;
        };
        let Ok(dir) = child(&root, &transfer.id, true) else {
            return;
        };
        for index in 0..transfer.files.len() {
            let name = std::ffi::CString::new(index.to_string()).unwrap();
            // unlinkat removes a link itself, never its target; directories are left alone.
            unsafe {
                libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0);
            }
        }
        let name = std::ffi::CString::new(transfer.id.as_str()).unwrap();
        unsafe {
            libc::unlinkat(root.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR);
        }
    }
}

impl Transfer {
    fn write(&mut self, index: usize, offset: u64, bytes: &[u8]) -> Result<(), String> {
        let file = self.files.get(index).ok_or("Invalid attachment file index")?;
        let (writer, written, hash) = &mut self.writers[index];
        if bytes.len() > CHUNK_BYTES || offset != *written || bytes.len() as u64 > file.size.saturating_sub(*written) {
            return Err("Invalid attachment offset or size".into());
        }
        writer.write_all(bytes).map_err(error)?;
        hash.update(bytes);
        *written += bytes.len() as u64;
        self.touched = Instant::now();
        Ok(())
    }
    fn finish(&mut self, exchange: Option<&str>) -> Result<Vec<Receipt>, String> {
        if self.exchange {
            let root = File::open(crate::paths::expand(exchange.ok_or("No exchange configured")?)).map_err(error)?;
            let dir = child(&child(&root, ".atomics-attachments", true)?, &self.id, true)?;
            for index in 0..self.files.len() {
                let mut source = child(&dir, &index.to_string(), false)?;
                if source.metadata().map_err(error)?.len() != self.files[index].size {
                    return Err("Attachment size changed".into());
                }
                let mut buffer = [0u8; CHUNK_BYTES];
                loop {
                    let count = source.read(&mut buffer).map_err(error)?;
                    if count == 0 {
                        break;
                    }
                    self.write(index, self.writers[index].1, &buffer[..count])?;
                }
            }
        }
        for (file, (writer, count, hash)) in self.files.iter().zip(&mut self.writers) {
            if *count != file.size || format!("{:x}", hash.clone().finalize()) != file.sha256 {
                return Err("Attachment SHA-256 or size mismatch; nothing sent to the chief".into());
            }
            writer.sync_all().map_err(error)?;
        }
        let receipts = self
            .files
            .iter()
            .enumerate()
            .map(|(i, file)| Receipt {
                file: file.clone(),
                path: self.dir.path().join(i.to_string()).join(&file.name).to_string_lossy().into(),
            })
            .collect();
        // Keep only verified bytes. The caller must discard them if chief delivery fails.
        self.dir.disable_cleanup(true);
        Ok(receipts)
    }
}

fn receipt_dir(home: &Path, files: &[Receipt]) -> Option<PathBuf> {
    let path = Path::new(&files.first()?.path).parent()?.parent()?;
    let root = home.canonicalize().ok()?.join("attachments");
    (path.parent() == Some(root.as_path()) && path.file_name()?.to_str()?.starts_with("drop-"))
        .then(|| path.to_path_buf())
}

pub fn discard(home: &Path, files: &[Receipt]) {
    if let Some(dir) = receipt_dir(home, files) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

pub fn set_owner(home: &Path, files: &[Receipt], plan: Option<&str>) -> Result<(), String> {
    let dir = receipt_dir(home, files).ok_or("Invalid attachment receipt")?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join("owner.json"))
        .map_err(error)?;
    file.write_all(&serde_json::to_vec(&plan).map_err(error)?).map_err(error)
}

/// Called by the existing plan trace cleanup hook; chief-only files are kept with chief history.
pub fn remove_plan(home: &Path, plan: &str) {
    let Ok(root) = private_root(home) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let path = entry.path().join("owner.json");
        if let Ok(bytes) = read_local(&path, 1024)
            && serde_json::from_slice::<Option<String>>(&bytes).ok().flatten().as_deref() == Some(plan)
        {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Match the trace archive's existing size-based rotation, including the unscoped chief trace.
pub fn prune_trace(path: &Path) {
    let Some(home) = path.parent().and_then(Path::parent) else {
        return;
    };
    let Ok(home) = home.canonicalize() else {
        return;
    };
    let Some(scope) = path.file_stem().and_then(|s| s.to_str()) else {
        return;
    };
    let plan = (scope != "chief").then_some(scope);
    let Ok(bytes) = read_local(path, crate::trace::MAX_DISK_BYTES as u64) else {
        return;
    };
    let mut keep = std::collections::HashSet::new();
    for line in bytes.split(|b| *b == b'\n') {
        if let Ok(entry) = serde_json::from_slice::<crate::trace::TraceEntry>(line)
            && entry.op == crate::trace::TraceOp::Attachment
            && let Ok(file) = serde_json::from_str::<Receipt>(&entry.summary)
            && let Some(dir) = receipt_dir(&home, &[file])
        {
            keep.insert(dir);
        }
    }
    let Ok(entries) = std::fs::read_dir(home.join("attachments")) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) || keep.contains(&entry.path()) {
            continue;
        }
        if let Ok(bytes) = read_local(&entry.path().join("owner.json"), 1024)
            && let Ok(owner) = serde_json::from_slice::<Option<String>>(&bytes)
            && owner.as_deref() == plan
        {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    fn info(bytes: &[u8]) -> FileInfo {
        FileInfo { name: "../data.txt".into(), size: bytes.len() as u64, sha256: hash(bytes) }
    }
    fn begin(inbox: &mut Inbox, bytes: &[u8], exchange: bool) -> String {
        inbox.request(1, Request::Begin { files: vec![info(bytes)], exchange }).unwrap().id
    }
    fn chunk(inbox: &mut Inbox, id: &str, bytes: &[u8]) -> Result<Reply, String> {
        inbox.request(
            1,
            Request::Chunk {
                id: id.into(),
                file: 0,
                offset: 0,
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
            },
        )
    }
    #[test]
    fn attachment_chunk_integrity_and_fresh_paths() {
        let home = tempfile::tempdir().unwrap();
        let mut inbox = Inbox::new(home.path(), Config::default());
        let id = begin(&mut inbox, b"abc", false);
        chunk(&mut inbox, &id, b"xyz").unwrap();
        assert!(inbox.request(1, Request::Commit { id }).unwrap_err().contains("SHA-256"));
        let mut paths = Vec::new();
        for _ in 0..2 {
            let id = begin(&mut inbox, b"abc", false);
            chunk(&mut inbox, &id, b"abc").unwrap();
            let reply = inbox.request(1, Request::Commit { id }).unwrap();
            let path = PathBuf::from(&reply.files[0].path);
            assert_eq!(std::fs::read(&path).unwrap(), b"abc");
            assert!(path.starts_with(home.path().canonicalize().unwrap().join("attachments")));
            assert_eq!(path.file_name().unwrap(), "_data.txt");
            paths.push(path);
        }
        assert_ne!(paths[0], paths[1]);
    }
    #[test]
    fn attachment_exchange_tampering_symlinks_and_folders_are_refused() {
        let home = tempfile::tempdir().unwrap();
        let exchange = tempfile::tempdir().unwrap();
        let config = Config { exchange: Some(exchange.path().to_string_lossy().into()), ..Config::default() };
        let mut inbox = Inbox::new(home.path(), config);
        for kind in ["tampered", "symlink", "folder", "valid"] {
            let id = begin(&mut inbox, b"abc", true);
            let dir = exchange.path().join(".atomics-attachments").join(&id);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("0");
            match kind {
                "symlink" => {
                    let target = home.path().join("target");
                    std::fs::write(&target, b"abc").unwrap();
                    std::os::unix::fs::symlink(target, &path).unwrap();
                }
                "folder" => std::fs::create_dir(&path).unwrap(),
                "tampered" => std::fs::write(&path, b"xyz").unwrap(),
                _ => std::fs::write(&path, b"abc").unwrap(),
            }
            let result = inbox.request(1, Request::Commit { id });
            assert_eq!(result.is_ok(), kind == "valid", "{kind}: {result:?}");
            if kind == "valid" {
                assert!(!path.exists(), "consumed exchange copy removed");
            }
        }
    }
    #[test]
    fn attachment_owner_offsets_count_size_and_disconnect() {
        let home = tempfile::tempdir().unwrap();
        let config = Config { max_file_bytes: 3, max_files: 1, max_staging_bytes: 3, ..Config::default() };
        let mut inbox = Inbox::new(home.path(), config);
        let id = begin(&mut inbox, b"abc", false);
        assert!(inbox.request(2, Request::Commit { id: id.clone() }).is_err());
        assert!(inbox.request(2, Request::Begin { files: vec![info(b"abc")], exchange: false }).is_err());
        assert!(inbox.request(1, Request::Begin { files: vec![info(b"abc")], exchange: false }).is_err());
        assert!(chunk(&mut inbox, &id, b"abcd").is_err());
        inbox.disconnect(1);
        let id = begin(&mut inbox, b"abc", false);
        chunk(&mut inbox, &id, b"abc").unwrap();
        assert!(chunk(&mut inbox, &id, b"abc").is_err(), "replay refused");
        inbox.disconnect(1);
        assert!(inbox.request(1, Request::Commit { id }).is_err());
        assert!(inbox.request(1, Request::Begin { files: vec![info(b"abcd")], exchange: false }).is_err());
        assert!(inbox.request(1, Request::Begin { files: vec![info(b"a"), info(b"b")], exchange: false }).is_err());
    }

    #[test]
    fn attachment_trace_rotation_preserves_then_removes_verified_files() {
        let home = tempfile::tempdir().unwrap();
        let mut inbox = Inbox::new(home.path(), Config::default());
        let id = begin(&mut inbox, b"abc", false);
        chunk(&mut inbox, &id, b"abc").unwrap();
        let files = inbox.request(1, Request::Commit { id }).unwrap().files;
        set_owner(home.path(), &files, None).unwrap();
        let path = crate::trace::disk::path(home.path(), None).unwrap();
        let mut log = crate::trace::TraceLog::default();
        let entry = log.attachment("chief", None, &files[0], 1);
        crate::trace::disk::append(&path, &entry).unwrap();
        assert_eq!(std::fs::read(&files[0].path).unwrap(), b"abc");
        // Put the attachment in the half that the existing archive rotation drops.
        let mut archive = OpenOptions::new().append(true).open(&path).unwrap();
        archive.write_all(&vec![b'\n'; crate::trace::MAX_DISK_BYTES]).unwrap();
        drop(archive);
        let mut next = entry;
        next.op = crate::trace::TraceOp::Other;
        next.summary = "Later trace entry".into();
        crate::trace::disk::append(&path, &next).unwrap();
        assert!(!Path::new(&files[0].path).exists());
    }

    #[test]
    fn attachment_timeouts_cancel_and_local_file_kinds() {
        let home = tempfile::tempdir().unwrap();
        let mut inbox = Inbox::new(home.path(), Config::default());
        let id = begin(&mut inbox, b"abc", false);
        inbox.config.idle_secs = 0;
        assert!(inbox.request(1, Request::Commit { id }).is_err());
        inbox.config.idle_secs = 120;
        let id = begin(&mut inbox, b"abc", false);
        inbox.request(1, Request::Cancel { id: id.clone() }).unwrap();
        assert!(inbox.request(1, Request::Commit { id }).is_err());
        let file = home.path().join("input");
        std::fs::write(&file, b"abc").unwrap();
        assert_eq!(read_local(&file, 3).unwrap(), b"abc");
        assert!(read_local(&file, 2).is_err());
        let link = home.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(read_local(&link, 3).is_err());
        assert!(read_local(home.path(), 3).is_err());
        let fifo = home.path().join("fifo");
        let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(read_local(&fifo, 3).is_err(), "a device must neither block nor be read");
    }
}
