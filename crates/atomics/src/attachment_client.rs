//! Native file input adapter. Never called by an agent channel.
use atomics_core::{
    attachments::{self, Config, FileInfo},
    paths,
};
use atomics_ui::attachments::{Preparation, Prepared, Source, Staging};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};

pub struct NativeSource {
    pub exchange: Option<PathBuf>,
}

impl Source for NativeSource {
    fn pick(&self) -> Option<atomics_ui::attachments::Picking> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let files = rfd::FileDialog::new().set_title("Files for the chief").pick_files().unwrap_or_default();
            let files = files.into_iter().map(|path| Arc::new(LocalFile(path)) as egui::DroppedFileHandle).collect();
            let _ = tx.send(files);
        });
        Some(rx)
    }
    fn prepare(&self, files: Vec<egui::DroppedFileHandle>, limits: Config) -> Preparation {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = (|| {
                if files.is_empty() || files.len() > limits.max_files {
                    return Err("Too many files in this drop".into());
                }
                let mut prepared = Vec::new();
                let mut total = 0u64;
                for file in files {
                    let path = file.path();
                    let bytes: Arc<[u8]> = attachments::disk::read_local(path, limits.max_file_bytes)?.into();
                    total = total.checked_add(bytes.len() as u64).ok_or("Drop is too large")?;
                    if total > limits.max_drop_bytes {
                        return Err("Drop exceeds the attachment byte limit".into());
                    }
                    let info = FileInfo {
                        name: attachments::safe_name(&path.file_name().unwrap_or_default().to_string_lossy()),
                        size: bytes.len() as u64,
                        sha256: attachments::hash(&bytes),
                    };
                    prepared.push(Prepared { info, bytes });
                }
                Ok(prepared)
            })();
            let _ = tx.send(result);
        });
        rx
    }
    fn exchange(&self, limits: &Config) -> bool {
        self.exchange
            .as_ref()
            .is_some_and(|root| limits.exchange.as_deref().is_some_and(|configured| *root == paths::expand(configured)))
    }
    fn stage(&self, id: &str, files: &[Prepared], limits: &Config) -> Staging {
        let (tx, rx) = mpsc::channel();
        let root = self.exchange.clone();
        let allowed = self.exchange(limits);
        let id = id.to_string();
        let files = files.to_vec();
        std::thread::spawn(move || {
            let result = (|| -> Result<(), String> {
                if !allowed
                    || !id.starts_with("drop-")
                    || id.len() > 128
                    || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                {
                    return Err("Invalid exchange transfer".into());
                }
                let root = root.ok_or("No exchange share")?.join(".atomics-attachments");
                match std::fs::DirBuilder::new().mode(0o700).create(&root) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e.to_string()),
                }
                // Open the parent without following a VM-created link; all writes are descriptor-relative.
                use std::os::fd::{AsRawFd, FromRawFd};
                let parent = std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
                    .open(&root)
                    .map_err(|e| e.to_string())?;
                let name = std::ffi::CString::new(id).map_err(|e| e.to_string())?;
                if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                let fd = unsafe {
                    libc::openat(
                        parent.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                let folder = unsafe { std::fs::File::from_raw_fd(fd) };
                for (i, file) in files.iter().enumerate() {
                    let name = std::ffi::CString::new(i.to_string()).unwrap();
                    let fd = unsafe {
                        libc::openat(
                            folder.as_raw_fd(),
                            name.as_ptr(),
                            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                            0o600,
                        )
                    };
                    if fd < 0 {
                        return Err(std::io::Error::last_os_error().to_string());
                    }
                    let mut output = unsafe { std::fs::File::from_raw_fd(fd) };
                    output.write_all(&file.bytes).and_then(|_| output.sync_all()).map_err(|e| e.to_string())?;
                }
                Ok(())
            })();
            let _ = tx.send(result);
        });
        rx
    }
}

#[derive(Debug)]
struct LocalFile(PathBuf);
impl egui::DroppedFile for LocalFile {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
    fn bytes(&self) -> Result<Vec<u8>, String> {
        attachments::disk::read_local(&self.0, Config::default().max_file_bytes)
    }
}
