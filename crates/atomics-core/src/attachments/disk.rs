//! Native storage for host-verified attachments.
mod storage;
use super::*;
use base64::Engine;
use rustix::fs::{self, AtFlags, Dir, Mode, OFlags};
use std::collections::{BTreeMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;
pub use storage::{delete, storage};

fn error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn component(name: &str) -> Result<&str, String> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        return Err("Invalid attachment component".into());
    }
    Ok(name)
}

fn child(parent: &File, name: &str, directory: bool) -> Result<File, String> {
    let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    let flags = if directory { flags | OFlags::DIRECTORY } else { flags };
    let file = File::from(fs::openat(parent, component(name)?, flags, Mode::empty()).map_err(error)?);
    if !directory && !file.metadata().map_err(error)?.is_file() {
        return Err("Regular files only".into());
    }
    Ok(file)
}

fn folder(parent: &File, name: &str, existing: bool) -> Result<File, String> {
    match fs::mkdirat(parent, component(name)?, Mode::from_raw_mode(0o700)) {
        Ok(()) => {}
        Err(rustix::io::Errno::EXIST) if existing => {}
        Err(e) => return Err(error(e)),
    }
    child(parent, name, true)
}

fn create(parent: &File, name: &str) -> Result<File, String> {
    fs::openat(
        parent,
        component(name)?,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map(File::from)
    .map_err(error)
}

fn names(dir: &File) -> Result<Vec<String>, String> {
    Dir::read_from(dir)
        .map_err(error)?
        .map(|entry| entry.map_err(error))
        .filter_map(|entry| match entry {
            Ok(entry) => {
                entry.file_name().to_str().ok().filter(|name| *name != "." && *name != "..").map(|n| Ok(n.to_owned()))
            }
            Err(e) => Some(Err(e)),
        })
        .collect()
}

fn private_root(home: &Path) -> Result<File, String> {
    std::fs::create_dir_all(home).map_err(error)?;
    let root = folder(&File::open(home).map_err(error)?, "attachments", true)?;
    fs::fchmod(&root, Mode::from_raw_mode(0o700)).map_err(error)?;
    Ok(root)
}

pub fn open_local(path: &Path) -> Result<File, String> {
    let file =
        OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path).map_err(error)?;
    if !file.metadata().map_err(error)?.is_file() {
        return Err("Regular files only (no folders, links or devices)".into());
    }
    Ok(file)
}

pub fn snapshot(file: File, limit: u64) -> Result<Vec<u8>, String> {
    let size = file.metadata().map_err(error)?.len();
    if size > limit {
        return Err("File exceeds the byte limit".into());
    }
    let mut bytes = Vec::new();
    file.take(size.saturating_add(1)).read_to_end(&mut bytes).map_err(error)?;
    if bytes.len() as u64 != size {
        return Err("File size changed while reading".into());
    }
    Ok(bytes)
}

fn read_local(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    snapshot(open_local(path)?, limit)
}

fn read_json<T: serde::de::DeserializeOwned>(dir: &File, name: &str) -> Result<T, String> {
    serde_json::from_reader(child(dir, name, false)?).map_err(error)
}
fn write_json(dir: &File, name: &str, value: &impl Serialize) -> Result<(), String> {
    let mut file = create(dir, name)?;
    serde_json::to_writer(&mut file, value).map_err(error)?;
    file.sync_all().map_err(error)
}

fn nonce() -> Result<String, String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(error)?;
    Ok(format!("{:032x}", u128::from_ne_bytes(bytes)))
}
fn owned_id(id: &str) -> Option<(&str, &str)> {
    let (host, nonce) = id.strip_prefix("drop-")?.split_once('-')?;
    [host, nonce].iter().all(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())).then_some((host, nonce))
}
fn drop_id(name: &str) -> bool {
    if owned_id(name).is_some() {
        return true;
    }
    name.strip_prefix("drop-")
        .is_some_and(|tail| matches!(tail.len(), 16 | 32) && tail.bytes().all(|c| c.is_ascii_alphanumeric()))
}
fn numbered(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|c| c.is_ascii_digit())
}

/// Removes just the numbered files from this transfer, relative to the checked share directory.
fn clear_exchange(parent: &File, id: &str) {
    let Ok(dir) = child(parent, id, true) else { return };
    for name in names(&dir).unwrap_or_default().into_iter().filter(|n| numbered(n)) {
        let _ = fs::unlinkat(&dir, name.as_str(), AtFlags::empty());
        let _ = fs::unlinkat(&dir, name.as_str(), AtFlags::REMOVEDIR);
    }
    let _ = fs::unlinkat(parent, id, AtFlags::REMOVEDIR);
}

/// Both ends own cleanup. The worker transfers this guard to the UI through its completion channel.
pub struct ExchangeStage {
    parent: File,
    dir: File,
    id: String,
}
impl ExchangeStage {
    pub fn new(root: &Path, id: &str) -> Result<Self, String> {
        if !drop_id(id) {
            return Err("Invalid exchange transfer".into());
        }
        let parent = folder(&File::open(root).map_err(error)?, ".atomics-attachments", true)?;
        let dir = folder(&parent, id, false)?;
        Ok(Self { parent, dir, id: id.into() })
    }
    pub fn write(&self, index: usize, bytes: &[u8]) -> Result<(), String> {
        let mut file = create(&self.dir, &index.to_string())?;
        file.write_all(bytes).and_then(|_| file.sync_all()).map_err(error)
    }
}
impl Drop for ExchangeStage {
    fn drop(&mut self) {
        clear_exchange(&self.parent, &self.id);
    }
}

struct Transfer {
    id: String,
    path: PathBuf,
    dir: File,
    files: Vec<FileInfo>,
    writers: Vec<(File, u64, Sha256)>,
    exchange: Option<File>,
    keep: bool,
    min_free: u64,
}
impl Drop for Transfer {
    fn drop(&mut self) {
        if let Some(root) = &self.exchange
            && let Ok(parent) = child(root, ".atomics-attachments", true)
        {
            clear_exchange(&parent, &self.id);
        }
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

struct Slot {
    id: String,
    started: Instant,
    touched: Mutex<Instant>,
    transfer: Mutex<Option<Transfer>>,
    reserved: u64,
    budget: Arc<AtomicU64>,
}
impl Drop for Slot {
    fn drop(&mut self) {
        drop(self.transfer.get_mut().unwrap().take());
        self.budget.fetch_sub(self.reserved, Ordering::Relaxed);
    }
}

#[derive(Default)]
pub struct ExchangeSweep {
    roots: Vec<PathBuf>,
    current: Option<(File, Dir)>,
}

pub struct Inbox {
    home: PathBuf,
    pub config: Config,
    pending: Mutex<BTreeMap<u64, Arc<Slot>>>,
    used: Arc<AtomicU64>,
    exchange_roots: Mutex<HashSet<PathBuf>>,
    exchange_id: std::sync::OnceLock<Result<String, String>>,
    recovery: Result<(), String>,
    commits: AtomicU64,
}

#[derive(Serialize, Deserialize)]
struct StagingOwner {
    plan: Option<String>,
    exchange: Option<PathBuf>,
}

impl Inbox {
    pub fn new(home: &Path, config: Config) -> Self {
        let mut inbox = Self {
            home: home.into(),
            config,
            pending: Default::default(),
            used: Default::default(),
            exchange_roots: Default::default(),
            exchange_id: Default::default(),
            recovery: Ok(()),
            commits: AtomicU64::new(0),
        };
        inbox.recovery = inbox.recover();
        let mut sweep = ExchangeSweep::default();
        while !inbox.sweep_exchange(&inbox.config, &mut sweep) {}
        inbox
    }
    fn recover(&mut self) -> Result<(), String> {
        if !self.home.join("attachments").try_exists().map_err(error)? {
            return Ok(());
        }
        let root = private_root(&self.home)?;
        let entries = names(&root)?;
        for name in entries.iter().filter(|name| {
            name.strip_prefix("share-").is_some_and(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        }) {
            fs::unlinkat(&root, name.as_str(), AtFlags::empty()).map_err(error)?;
        }
        for name in entries.iter().filter(|name| name.starts_with("exchange-root-")) {
            self.exchange_roots.get_mut().unwrap().insert(read_json(&root, name)?);
        }
        for id in entries.into_iter().filter(|id| drop_id(id)) {
            let dir = child(&root, &id, true)?;
            if let Ok(plan) = read_json::<Option<String>>(&dir, "owner.json") {
                // A restart loses in-memory delivery acknowledgements. Recover only receipts
                // already appended to the provider's archive; discard interrupted deliveries.
                if let Ok(files) = read_json::<Vec<Receipt>>(&dir, "verified.json") {
                    let mut archived = None;
                    for (i, file) in files.iter().enumerate() {
                        let marker = format!("traced-{i}");
                        if child(&dir, &marker, false).is_ok() || !Path::new(&file.path).try_exists().map_err(error)? {
                            continue;
                        }
                        if archived.is_none() {
                            let archive = crate::trace::disk::path(&self.home, plan.as_deref()).map_err(error)?;
                            archived = Some(if archive.try_exists().map_err(error)? {
                                archive_files(&archive)?
                            } else {
                                HashSet::new()
                            });
                        }
                        if archived.as_ref().is_some_and(|keep| keep.contains(Path::new(&file.path))) {
                            create(&dir, &marker)?;
                        } else {
                            std::fs::remove_dir_all(self.home.join("attachments").join(&id).join(i.to_string()))
                                .map_err(error)?;
                        }
                    }
                    if !names(&dir)?.iter().any(|n| numbered(n)) {
                        std::fs::remove_dir_all(self.home.join("attachments").join(&id)).map_err(error)?;
                    }
                }
                continue;
            }
            if let Ok(owner) = read_json::<StagingOwner>(&dir, "staging.json")
                && let Some(path) = owner.exchange
            {
                self.exchange_roots.get_mut().unwrap().insert(path);
            }
            // Charge any bytes the OS could not reclaim. If they cannot be measured, recovery
            // fails closed rather than granting the same staging space to another transfer.
            if std::fs::remove_dir_all(self.home.join("attachments").join(id)).is_err() {
                let mut bytes = self.used.load(Ordering::Relaxed);
                for index in names(&dir)?.iter().filter(|n| numbered(n)) {
                    let folder = child(&dir, index, true)?;
                    for name in names(&folder)? {
                        bytes = bytes.saturating_add(child(&folder, &name, false)?.metadata().map_err(error)?.len());
                    }
                }
                self.used.store(bytes, Ordering::Relaxed);
            }
        }
        Ok(())
    }
    // Remember shares that may receive a late client write even after cancellation and restart.
    fn remember_exchange(&self, path: &Path) -> Result<(), String> {
        let dir = private_root(&self.home)?;
        let key = format!("exchange-root-{}", hash(path.as_os_str().as_encoded_bytes()));
        if child(&dir, &key, false).is_err() {
            let temp = format!("share-{}", nonce()?);
            write_json(&dir, &temp, &path)?;
            fs::renameat(&dir, temp.as_str(), &dir, key.as_str()).map_err(error)?;
        }
        self.exchange_roots.lock().unwrap().insert(path.to_owned());
        Ok(())
    }
    // Independent hosts may use the same share. Only sweep names minted by this host.
    fn exchange_id(&self) -> Result<&str, String> {
        self.exchange_id
            .get_or_init(|| {
                let root = private_root(&self.home)?;
                if let Ok(id) = read_json::<String>(&root, "exchange-owner.json") {
                    return Ok(id);
                }
                let id = nonce()?;
                write_json(&root, "exchange-owner.json", &id)?;
                Ok(id)
            })
            .as_deref()
            .map_err(Clone::clone)
    }

    /// Inspect at most 64 directory entries per tick, resuming the same directory next time.
    /// Startup drives this to completion before admitting any clients.
    pub fn sweep_exchange(&self, config: &Config, sweep: &mut ExchangeSweep) -> bool {
        if sweep.current.is_none() && sweep.roots.is_empty() {
            let mut roots = self.exchange_roots.lock().unwrap();
            if let Some(root) = &config.exchange {
                roots.insert(crate::paths::expand(root));
            }
            sweep.roots.extend(roots.iter().cloned());
        }
        for _ in 0..64 {
            if let Some((parent, entries)) = &mut sweep.current {
                let Some(entry) = entries.next() else {
                    sweep.current = None;
                    continue;
                };
                let Ok(entry) = entry else { continue };
                let Ok(id) = entry.file_name().to_str() else { continue };
                if !drop_id(id) {
                    continue;
                }
                let stale = match owned_id(id) {
                    Some((owner, _)) => self.exchange_id().is_ok_and(|host| host == owner),
                    None => child(parent, id, true)
                        .ok()
                        .and_then(|d| d.metadata().ok()?.modified().ok()?.elapsed().ok())
                        .is_some_and(|age| age.as_secs() >= config.timeout_secs.max(600)),
                };
                if stale && !self.pending.lock().unwrap().values().any(|s| s.id == id) {
                    clear_exchange(parent, id);
                }
            } else if let Some(path) = sweep.roots.pop() {
                if let Ok(root) = File::open(path)
                    && let Ok(parent) = child(&root, ".atomics-attachments", true)
                    && let Ok(entries) = Dir::read_from(&parent)
                {
                    sweep.current = Some((parent, entries));
                }
            } else {
                return true;
            }
        }
        sweep.current.is_none() && sweep.roots.is_empty()
    }
    pub fn expire(&self, config: &Config, after: &mut Option<u64>) {
        use std::ops::Bound::{Excluded, Unbounded};
        let mut pending = self.pending.lock().unwrap();
        let batch = pending.range((after.map_or(Unbounded, Excluded), Unbounded)).take(32);
        let mut expired = Vec::new();
        *after = None;
        for (&owner, slot) in batch {
            *after = Some(owner);
            if slot.started.elapsed().as_secs() >= config.timeout_secs
                || slot.touched.lock().unwrap().elapsed().as_secs() >= config.idle_secs
            {
                expired.push(owner);
            }
        }
        if after.as_ref() == pending.last_key_value().map(|(owner, _)| owner) {
            *after = None;
        }
        let removed: Vec<_> = expired.into_iter().filter_map(|k| pending.remove(&k)).collect();
        drop(pending);
        drop(removed);
    }
    pub fn disconnect(&self, owner: u64) {
        let removed = self.pending.lock().unwrap().remove(&owner);
        drop(removed);
    }
    pub fn request(&self, owner: u64, request: Request) -> Result<Reply, String> {
        self.request_with(owner, request, &self.config)
    }
    pub fn request_with(&self, owner: u64, request: Request, config: &Config) -> Result<Reply, String> {
        let mut reply =
            Reply { queued: false, limits: None, id: String::new(), error: None, complete: false, files: vec![] };
        if matches!(request, Request::Limits) {
            reply.limits = Some(config.clone());
            return Ok(reply);
        }
        self.recovery.clone()?;
        if let Request::Begin { files, exchange, plan } = request {
            let bytes = validate(&files, config)?;
            if let Some(plan) = &plan {
                crate::trace::disk::path(&self.home, Some(plan)).map_err(error)?;
            }
            let exchange = exchange
                .then(|| {
                    config
                        .exchange
                        .as_deref()
                        .map(crate::paths::expand)
                        .ok_or("No exchange share configured on this host")
                })
                .transpose()?;
            if let Some(path) = &exchange {
                self.remember_exchange(path)?;
            }
            let id = if exchange.is_some() {
                format!("drop-{}-{}", self.exchange_id()?, nonce()?)
            } else {
                format!("drop-{}", nonce()?)
            };
            let now = Instant::now();
            let root = private_root(&self.home)?;
            let slot = loop {
                // Scan outside the registry lock. Retry if a commit moved bytes from a
                // reservation into retained storage while this scan was in progress.
                let generation = self.commits.load(Ordering::Acquire);
                let retained = storage::retained(&root)?;
                let free = storage::free_bytes(&root)?;
                let mut pending = self.pending.lock().unwrap();
                if pending.contains_key(&owner) {
                    return Err("An attachment is already uploading".into());
                }
                if generation != self.commits.load(Ordering::Acquire) {
                    continue;
                }
                let reserved = self.used.load(Ordering::Relaxed);
                let required = reserved.checked_add(bytes).ok_or("Attachment reservation overflow")?;
                if required > config.max_retained_bytes.saturating_sub(retained) {
                    return Err("Retained attachment storage is full; delete retained files to free space".into());
                }
                if free < config.min_free_bytes || required > free - config.min_free_bytes {
                    return Err("Insufficient free space for attachments".into());
                }
                if required > config.max_staging_bytes {
                    return Err("Host attachment staging is full".into());
                }
                self.used.fetch_add(bytes, Ordering::Relaxed);
                let slot = Arc::new(Slot {
                    id: id.clone(),
                    started: now,
                    touched: Mutex::new(now),
                    reserved: bytes,
                    budget: self.used.clone(),
                    transfer: Mutex::new(None),
                });
                pending.insert(owner, slot.clone());
                break slot;
            };
            let result =
                Transfer::begin(&self.home, &id, files, StagingOwner { plan, exchange }, config.min_free_bytes);
            match result {
                Ok(transfer) => *slot.transfer.lock().unwrap() = Some(transfer),
                Err(error) => {
                    self.remove(owner, &slot);
                    return Err(error);
                }
            }
            reply.id = id;
            return Ok(reply);
        }
        let id = match &request {
            Request::Chunk { id, .. } | Request::Commit { id } | Request::Cancel { id } => id,
            _ => unreachable!(),
        };
        let slot = self
            .pending
            .lock()
            .unwrap()
            .get(&owner)
            .filter(|s| s.id == *id)
            .cloned()
            .ok_or("Attachment is absent or belongs to another connection")?;
        reply.id.clone_from(id);
        if slot.started.elapsed().as_secs() >= config.timeout_secs
            || slot.touched.lock().unwrap().elapsed().as_secs() >= config.idle_secs
        {
            self.remove(owner, &slot);
            return Err("Attachment expired".into());
        }
        let mut guard = slot.transfer.lock().unwrap();
        let transfer = guard.as_mut().ok_or("Attachment is not ready")?;
        let result = (|| {
            match request {
                Request::Chunk { file, offset, data, .. } => {
                    if transfer.exchange.is_some() || data.len() > CHUNK_BYTES.div_ceil(3) * 4 {
                        return Err("Unexpected or oversized attachment chunk".into());
                    }
                    let bytes = base64::engine::general_purpose::STANDARD.decode(data).map_err(error)?;
                    transfer.write(file, offset, &bytes)?;
                    *slot.touched.lock().unwrap() = Instant::now();
                }
                Request::Cancel { .. } => reply.complete = true,
                Request::Commit { .. } => {
                    reply.files = transfer.finish()?;
                    if !self.pending.lock().unwrap().get(&owner).is_some_and(|s| Arc::ptr_eq(s, &slot))
                        || slot.started.elapsed().as_secs() >= config.timeout_secs
                    {
                        return Err("Attachment expired or disconnected".into());
                    }
                    self.commits.fetch_add(1, Ordering::Release);
                    transfer.keep = true;
                    reply.complete = true;
                }
                _ => unreachable!(),
            }
            Ok(())
        })();
        if result.is_err() || reply.complete {
            self.remove(owner, &slot);
        }
        result.map(|()| reply)
    }
    fn remove(&self, owner: u64, slot: &Arc<Slot>) {
        let mut pending = self.pending.lock().unwrap();
        if pending.get(&owner).is_some_and(|s| Arc::ptr_eq(s, slot)) {
            pending.remove(&owner);
        }
    }
    pub fn local_bytes(&self, owner: u64, id: &str, files: &[Arc<[u8]>]) -> Result<(), String> {
        let slot =
            self.pending.lock().unwrap().get(&owner).filter(|s| s.id == id).cloned().ok_or("Attachment expired")?;
        let mut guard = slot.transfer.lock().unwrap();
        let transfer = guard.as_mut().ok_or("Attachment absent")?;
        for (i, bytes) in files.iter().enumerate() {
            for (j, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
                transfer.write(i, (j * CHUNK_BYTES) as u64, chunk)?;
            }
        }
        *slot.touched.lock().unwrap() = Instant::now();
        Ok(())
    }
}

impl Transfer {
    fn begin(home: &Path, id: &str, files: Vec<FileInfo>, owner: StagingOwner, min_free: u64) -> Result<Self, String> {
        let root = private_root(home)?;
        let path = home.canonicalize().map_err(error)?.join("attachments").join(id);
        let exchange = owner.exchange.as_ref().map(File::open).transpose().map_err(error)?;
        let dir = folder(&root, id, false)?;
        let mut t = Self { id: id.into(), dir, path, files, writers: vec![], exchange, min_free, keep: false };
        write_json(&t.dir, "staging.json", &owner)?;
        for (i, file) in t.files.iter_mut().enumerate() {
            file.name = safe_name(&file.name);
            let dir = folder(&t.dir, &i.to_string(), false)?;
            t.writers.push((create(&dir, &file.name)?, 0, Sha256::new()));
        }
        Ok(t)
    }
    fn write(&mut self, index: usize, offset: u64, bytes: &[u8]) -> Result<(), String> {
        let file = self.files.get(index).ok_or("Invalid attachment file index")?;
        let (writer, written, hash) = &mut self.writers[index];
        if bytes.len() > CHUNK_BYTES || offset != *written || bytes.len() as u64 > file.size.saturating_sub(*written) {
            return Err("Invalid attachment offset or size".into());
        }
        let free = storage::free_bytes(writer)?;
        if free < self.min_free || bytes.len() as u64 > free - self.min_free {
            return Err("Insufficient free space for attachments".into());
        }
        writer.write_all(bytes).map_err(error)?;
        hash.update(bytes);
        *written += bytes.len() as u64;
        Ok(())
    }
    fn finish(&mut self) -> Result<Vec<Receipt>, String> {
        if let Some(root) = &self.exchange {
            let dir = child(&child(root, ".atomics-attachments", true)?, &self.id, true)?;
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
        let at =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
        let mut receipts = Vec::new();
        for (i, (file, (writer, count, hash))) in self.files.iter().zip(&self.writers).enumerate() {
            if *count != file.size || format!("{:x}", hash.clone().finalize()) != file.sha256 {
                return Err("Attachment SHA-256 or size mismatch; nothing sent to the chief".into());
            }
            writer.sync_all().map_err(error)?;
            receipts.push(Receipt {
                id: format!("{}-{i}", self.id),
                file: file.clone(),
                at,
                path: self.path.join(i.to_string()).join(&file.name).to_string_lossy().into(),
            });
        }
        write_json(&self.dir, "verified.json", &receipts)?;
        let owner: StagingOwner = read_json(&self.dir, "staging.json")?;
        write_json(&self.dir, "verified-owner.json", &owner.plan)?;
        Ok(receipts)
    }
}

fn receipt_dir(home: &Path, files: &[Receipt]) -> Option<PathBuf> {
    let path = Path::new(&files.first()?.path).parent()?.parent()?;
    let root = home.canonicalize().ok()?.join("attachments");
    (path.parent() == Some(root.as_path()) && drop_id(path.file_name()?.to_str()?)).then(|| path.to_path_buf())
}
pub fn discard(home: &Path, files: &[Receipt]) {
    if let Some(dir) = receipt_dir(home, files) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Publish the host's verified manifest only after the commit's final authorization check.
pub fn accept(home: &Path, files: &[Receipt]) -> Result<Option<String>, String> {
    let path = receipt_dir(home, files).ok_or("Invalid attachment receipt")?;
    let dir = File::open(path).map_err(error)?;
    let owner: StagingOwner = read_json(&dir, "staging.json")?;
    fs::renameat(&dir, "verified-owner.json", &dir, "owner.json").map_err(error)?;
    Ok(owner.plan)
}

pub fn verified(home: &Path, id: Option<&str>) -> Result<Vec<Receipt>, String> {
    if !home.join("attachments").try_exists().map_err(error)? {
        return Ok(vec![]);
    }
    let root = private_root(home)?;
    let mut records = Vec::new();
    for name in names(&root)?.into_iter().filter(|n| drop_id(n)) {
        if id.is_some_and(|id| id.rsplit_once('-').is_none_or(|(drop, _)| drop != name)) {
            continue;
        }
        let Ok(dir) = child(&root, &name, true) else { continue };
        if read_json::<Option<String>>(&dir, "owner.json").is_err() {
            continue;
        }
        // Drops saved by older versions have no verified manifest.
        if let Ok(files) = read_json::<Vec<Receipt>>(&dir, "verified.json") {
            records
                .extend(files.into_iter().filter(|f| id.is_none_or(|id| f.id == id) && Path::new(&f.path).is_file()));
        }
    }
    records.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| a.id.cmp(&b.id)));
    records.truncate(100);
    Ok(records)
}

pub fn remove_plan(home: &Path, plan: &str) {
    let Ok(entries) = std::fs::read_dir(home.join("attachments")) else { return };
    for entry in entries.flatten().filter(|e| drop_id(&e.file_name().to_string_lossy())) {
        if let Ok(dir) = File::open(entry.path())
            && read_json::<Option<String>>(&dir, "owner.json").ok().flatten().as_deref() == Some(plan)
        {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

fn archive_files(path: &Path) -> Result<HashSet<PathBuf>, String> {
    let bytes = read_local(path, crate::trace::MAX_DISK_BYTES as u64)?;
    let mut keep = HashSet::new();
    for line in bytes.split(|b| *b == b'\n') {
        if let Ok(entry) = serde_json::from_slice::<crate::trace::TraceEntry>(line)
            && entry.op == crate::trace::TraceOp::Attachment
            && let Ok(file) = serde_json::from_str::<Receipt>(&entry.summary)
        {
            keep.insert(PathBuf::from(file.path));
        }
    }
    Ok(keep)
}

pub fn prune_trace(path: &Path) {
    let Some(home) = path.parent().and_then(Path::parent).and_then(|p| p.canonicalize().ok()) else { return };
    let Some(scope) = path.file_stem().and_then(|s| s.to_str()) else { return };
    let plan = match scope {
        "@chief" => None,
        "@plan-chief" => Some("chief"),
        name => Some(name),
    };
    let Ok(keep) = archive_files(path) else { return };
    let Ok(entries) = std::fs::read_dir(home.join("attachments")) else { return };
    for drop in entries.flatten().filter(|e| drop_id(&e.file_name().to_string_lossy())) {
        let Ok(dir) = File::open(drop.path()) else { continue };
        if read_json::<Option<String>>(&dir, "owner.json").ok().as_ref().map(Option::as_deref) != Some(plan) {
            continue;
        }
        let Ok(items) = names(&dir) else { continue };
        for n in items.iter().filter(|n| numbered(n)) {
            // New receipts are protected while awaiting delivery and during a multi-file trace append.
            if items.iter().any(|s| s == "verified.json") && !items.contains(&format!("traced-{n}")) {
                continue;
            }
            let folder = drop.path().join(n);
            if !keep.iter().any(|path| path.parent() == Some(&folder)) {
                let _ = std::fs::remove_dir_all(folder);
            }
        }
        if names(&dir).is_ok_and(|names| !names.iter().any(|n| numbered(n))) {
            let _ = std::fs::remove_dir_all(drop.path());
        }
    }
}
pub fn traced(home: &Path, file: &Receipt) -> Result<(), String> {
    let path = receipt_dir(home, std::slice::from_ref(file)).ok_or("Invalid attachment receipt")?;
    let (_, index) = file.id.rsplit_once('-').ok_or("Invalid attachment id")?;
    create(&File::open(path).map_err(error)?, &format!("traced-{index}")).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    fn info(bytes: &[u8]) -> FileInfo {
        FileInfo { name: "../data.txt".into(), size: bytes.len() as u64, sha256: hash(bytes) }
    }
    fn begin(inbox: &mut Inbox, bytes: &[u8], exchange: bool) -> String {
        inbox.request(1, Request::Begin { plan: None, files: vec![info(bytes)], exchange }).unwrap().id
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
    fn attachment_creates_private_drop_before_receiving_bytes() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let mut inbox = Inbox::new(home.path(), Config::default());
        let id = begin(&mut inbox, b"abc", false);
        let dir = home.path().join("attachments").join(id);
        for (path, mode) in [(dir.clone(), 0o700), (dir.join("0"), 0o700), (dir.join("0/_data.txt"), 0o600)] {
            assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, mode);
        }
    }

    #[test]
    fn attachment_restart_recovers_interrupted_staging() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("attachments/drop-0123456789abcdef/0");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("partial"), b"abc").unwrap();
        let mut inbox = Inbox::new(home.path(), Config { max_staging_bytes: 3, ..Default::default() });
        begin(&mut inbox, b"abc", false);
        assert!(!dir.exists(), "old staging must be reclaimed before reusing its budget");
    }

    #[test]
    fn attachment_exchange_cleanup_remembers_begin_root() {
        let home = tempfile::tempdir().unwrap();
        let exchange = tempfile::tempdir().unwrap();
        let mut inbox = Inbox::new(
            home.path(),
            Config { exchange: Some(exchange.path().display().to_string()), ..Default::default() },
        );
        let id = begin(&mut inbox, b"abc", true);
        let dir = exchange.path().join(".atomics-attachments").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("0"), b"abc").unwrap();
        inbox.config.exchange = None;
        inbox.disconnect(1);
        assert!(!dir.exists());
    }

    #[test]
    fn attachment_chief_plan_and_unscoped_archive_are_distinct() {
        let home = tempfile::tempdir().unwrap();
        assert_ne!(
            crate::trace::disk::path(home.path(), None).unwrap(),
            crate::trace::disk::path(home.path(), Some("chief")).unwrap()
        );
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
        assert!(inbox.request(2, Request::Begin { plan: None, files: vec![info(b"abc")], exchange: false }).is_err());
        assert!(inbox.request(1, Request::Begin { plan: None, files: vec![info(b"abc")], exchange: false }).is_err());
        assert!(chunk(&mut inbox, &id, b"abcd").is_err());
        inbox.disconnect(1);
        let id = begin(&mut inbox, b"abc", false);
        chunk(&mut inbox, &id, b"abc").unwrap();
        assert!(chunk(&mut inbox, &id, b"abc").is_err(), "replay refused");
        inbox.disconnect(1);
        assert!(inbox.request(1, Request::Commit { id }).is_err());
        assert!(inbox.request(1, Request::Begin { plan: None, files: vec![info(b"abcd")], exchange: false }).is_err());
        assert!(
            inbox
                .request(1, Request::Begin { plan: None, files: vec![info(b"a"), info(b"b")], exchange: false })
                .is_err()
        );
    }

    #[test]
    fn attachment_trace_rotation_preserves_then_removes_verified_files() {
        let home = tempfile::tempdir().unwrap();
        let mut inbox = Inbox::new(home.path(), Config::default());
        let id = begin(&mut inbox, b"abc", false);
        chunk(&mut inbox, &id, b"abc").unwrap();
        let files = inbox.request(1, Request::Commit { id }).unwrap().files;
        accept(home.path(), &files).unwrap();
        traced(home.path(), &files[0]).unwrap();
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
    #[test]
    fn attachment_slow_transfer_does_not_block_other_sessions() {
        let home = tempfile::tempdir().unwrap();
        let mut inbox = Inbox::new(home.path(), Config::default());
        begin(&mut inbox, b"abc", false);
        let service = Arc::new(inbox);
        let slot = service.pending.lock().unwrap().get(&1).unwrap().clone();
        let io = slot.transfer.lock().unwrap();
        let other = service.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || tx.send(other.request(2, Request::Limits)).unwrap());
        let reply = rx.recv_timeout(std::time::Duration::from_secs(2));
        drop(io);
        worker.join().unwrap();
        assert!(reply.unwrap().unwrap().limits.is_some());
    }

    #[test]
    fn attachment_expiry_advances_past_fresh_transfers_in_bounded_batches() {
        let home = tempfile::tempdir().unwrap();
        let inbox = Inbox::new(home.path(), Config::default());
        for owner in 0..33 {
            inbox.request(owner, Request::Begin { plan: None, files: vec![info(b"")], exchange: false }).unwrap();
        }
        let last = inbox.pending.lock().unwrap().get(&32).unwrap().clone();
        *last.touched.lock().unwrap() = Instant::now() - std::time::Duration::from_secs(121);
        let config = Config { idle_secs: 120, ..Config::default() };
        let mut after = None;
        inbox.expire(&config, &mut after);
        assert_eq!(after, Some(31));
        assert!(inbox.pending.lock().unwrap().contains_key(&32));
        inbox.expire(&config, &mut after);
        assert_eq!(after, None);
        assert!(!inbox.pending.lock().unwrap().contains_key(&32));
    }

    #[test]
    fn attachment_rotation_keeps_only_the_retained_sibling() {
        let home = tempfile::tempdir().unwrap();
        let inbox = Inbox::new(home.path(), Config::default());
        let id = inbox
            .request(1, Request::Begin { plan: None, files: vec![info(b""), info(b"")], exchange: false })
            .unwrap()
            .id;
        let files = inbox.request(1, Request::Commit { id }).unwrap().files;
        accept(home.path(), &files).unwrap();
        for f in &files {
            traced(home.path(), f).unwrap();
        }
        let path = crate::trace::disk::path(home.path(), None).unwrap();
        let entry = crate::trace::TraceLog::default().attachment("conversation", None, &files[1], 1);
        crate::trace::disk::append(&path, &entry).unwrap();
        prune_trace(&path);
        assert!(!Path::new(&files[0].path).exists());
        assert!(Path::new(&files[1].path).is_file());
        assert_eq!(verified(home.path(), None).unwrap(), files[1..]);
    }
    #[test]
    fn attachment_retained_budget_and_free_space_survive_commit() {
        let home = tempfile::tempdir().unwrap();
        let config: Config = serde_json::from_value(serde_json::json!({"max_retained_bytes":3})).unwrap();
        let mut inbox = Inbox::new(home.path(), config);
        let id = begin(&mut inbox, b"abc", false);
        chunk(&mut inbox, &id, b"abc").unwrap();
        let files = inbox.request(1, Request::Commit { id }).unwrap().files;
        accept(home.path(), &files).unwrap();
        assert!(
            inbox.request(2, Request::Begin { plan: None, files: vec![info(b"x")], exchange: false }).is_err(),
            "committed bytes still occupy the quota"
        );
        traced(home.path(), &files[0]).unwrap();
        drop(inbox);
        let mut inbox = Inbox::new(home.path(), Config { max_retained_bytes: 3, ..Default::default() });
        assert!(inbox.request(2, Request::Begin { plan: None, files: vec![info(b"x")], exchange: false }).is_err());
        let inventory = storage(home.path(), None).unwrap();
        assert_eq!(inventory.retained_bytes, 3);
        assert_eq!(inventory.files[0].id, files[0].id);
        assert_eq!(delete(home.path(), &files[0].id).unwrap(), 3);
        assert!(verified(home.path(), Some(&files[0].id)).unwrap().is_empty());
        let _id = begin(&mut inbox, b"abc", false);
        inbox.disconnect(1);
        inbox.config = serde_json::from_value(serde_json::json!({"min_free_bytes":u64::MAX})).unwrap();
        assert!(
            inbox.request(2, Request::Begin { plan: None, files: vec![info(b"x")], exchange: false }).is_err(),
            "host free-space floor"
        );
    }
    #[test]
    fn attachment_janitor_recovers_late_exchange_staging_and_respects_other_hosts() {
        let home = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let exchange = tempfile::tempdir().unwrap();
        let config = Config { exchange: Some(exchange.path().display().to_string()), ..Default::default() };
        let mut inbox = Inbox::new(home.path(), config.clone());
        let mut foreign = Inbox::new(other.path(), config.clone());
        let active = begin(&mut foreign, b"abc", true);
        let active_guard = ExchangeStage::new(exchange.path(), &active).unwrap();
        active_guard.write(0, b"abc").unwrap();
        let id = begin(&mut inbox, b"abc", true);
        inbox.disconnect(1);
        let late = ExchangeStage::new(exchange.path(), &id).unwrap();
        late.write(0, b"abc").unwrap();
        let root = exchange.path().join(".atomics-attachments");
        // The client can die without running Drop. A restarted host sweeps its own namespace.
        drop(inbox);
        let restarted = Inbox::new(home.path(), Config::default());
        assert!(!root.join(&id).exists());
        assert!(root.join(&active).join("0").is_file());
        // The next bounded pass also cleans a transfer created after startup.
        let _late_again = ExchangeStage::new(exchange.path(), &id).unwrap();
        let mut sweep = ExchangeSweep::default();
        while !restarted.sweep_exchange(&config, &mut sweep) {}
        assert!(!root.join(&id).exists());
    }

    #[test]
    fn attachment_legacy_chief_archive_preserves_both_scopes() {
        let home = tempfile::tempdir().unwrap();
        let archive = home.path().join("trace/chief.jsonl");
        let mut log = crate::trace::TraceLog::default();
        let file = Receipt { id: String::new(), at: 0, path: "legacy".into(), file: info(b"") };
        for plan in [None, Some("chief")] {
            let entry = log.attachment("conversation", plan, &file, 1);
            crate::trace::disk::append(&archive, &entry).unwrap();
        }
        for plan in [Some("chief"), None] {
            let split = crate::trace::disk::path(home.path(), plan).unwrap();
            let loaded = crate::trace::disk::load(&split, &Default::default()).unwrap();
            assert_eq!(loaded.entries.len(), 1);
            assert_eq!(loaded.entries[0].plan_id.as_deref(), plan);
        }
    }
}
