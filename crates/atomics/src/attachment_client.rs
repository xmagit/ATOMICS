//! Native file input adapter. A selection owns open regular files until the worker snapshots them.
use atomics_core::{
    attachments::{self, Config, FileInfo, disk},
    paths,
};
use atomics_ui::attachments::{Picking, Prepared, Selection, Source, StageGuard, Staging};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};

pub struct NativeSource {
    pub exchange: Option<PathBuf>,
}

fn select(paths: impl IntoIterator<Item = PathBuf>) -> Result<Selection, String> {
    let mut files = Vec::new();
    for path in paths {
        let file = disk::open_local(&path)?;
        let name = attachments::safe_name(&path.file_name().unwrap_or_default().to_string_lossy());
        files.push((name, file));
    }
    Ok(Box::new(move |limits| {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = (|| {
                if files.is_empty() || files.len() > limits.max_files {
                    return Err("Too many files in this drop".into());
                }
                let mut remaining = limits.max_drop_bytes;
                let mut prepared = Vec::new();
                for (name, file) in files {
                    let bytes: Arc<[u8]> = disk::snapshot(file, limits.max_file_bytes.min(remaining))?.into();
                    let info = FileInfo { name, size: bytes.len() as u64, sha256: attachments::hash(&bytes) };
                    remaining -= info.size;
                    prepared.push(Prepared { info, bytes });
                }
                Ok(prepared)
            })();
            let _ = tx.send(result);
        });
        rx
    }))
}

impl Source for NativeSource {
    fn pick(&self) -> Option<Picking> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let files = rfd::FileDialog::new().set_title("Files for the chief").pick_files();
            let _ = tx.send(files.map(select).transpose());
        });
        Some(rx)
    }
    fn select(&self, files: Vec<egui::DroppedFileHandle>) -> Result<Selection, String> {
        select(files.into_iter().map(|f| f.path().to_owned()))
    }
    fn exchange(&self, limits: &Config) -> bool {
        self.exchange
            .as_ref()
            .is_some_and(|root| limits.exchange.as_deref().is_some_and(|configured| *root == paths::expand(configured)))
    }
    fn stage(&self, id: &str, files: &[Prepared], limits: &Config) -> Staging {
        let (tx, rx) = mpsc::channel();
        let root = self.exchange(limits).then(|| self.exchange.clone()).flatten();
        let files = files.to_vec();
        let id = id.to_owned();
        std::thread::spawn(move || {
            let result = (|| {
                let stage = disk::ExchangeStage::new(&root.ok_or("No exchange share")?, &id)?;
                for (i, file) in files.iter().enumerate() {
                    stage.write(i, &file.bytes)?;
                }
                Ok(Box::new(stage) as StageGuard)
            })();
            // If the window cancelled or disconnected while the worker ran, send drops the guard here.
            let _ = tx.send(result);
        });
        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn attachment_client_owns_the_staged_copy_until_completion_or_cancel() {
        let root = tempfile::tempdir().unwrap();
        let source = NativeSource { exchange: Some(root.path().into()) };
        let limits = Config { exchange: Some(root.path().display().to_string()), ..Default::default() };
        let id = "drop-0123456789abcdef";
        let guard = source.stage(id, &[], &limits).recv().unwrap().unwrap();
        let staging = root.path().join(".atomics-attachments").join(id);
        assert!(staging.exists());
        drop(guard);
        assert!(!staging.exists());
    }
}
