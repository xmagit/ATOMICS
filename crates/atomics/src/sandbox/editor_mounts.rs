use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};

use crate::terminal::{Shared, Terminal};
use atomics_core::{git, paths};

struct OpenEditor {
    file: PathBuf,
    terminal: Weak<Shared>,
}

impl OpenEditor {
    fn is_open(&self) -> bool {
        self.terminal.upgrade().is_some_and(|terminal| !terminal.exited.load(Ordering::Acquire))
    }
}

struct Access {
    editors: Vec<OpenEditor>,
    mounts: Vec<Weak<[PathBuf]>>,
}

static ACCESS: Mutex<Access> = Mutex::new(Access { editors: Vec::new(), mounts: Vec::new() });

pub(crate) fn open(file: &Path, start: impl FnOnce() -> Result<Terminal, String>) -> Result<Terminal, String> {
    let mut access = ACCESS.lock().unwrap();
    access.mounts.retain(|mount| mount.strong_count() != 0);
    for folders in access.mounts.iter().filter_map(Weak::upgrade) {
        if folders.iter().any(|folder| paths::at_or_below(file, folder)) {
            return Err(format!(
                "A sandbox is mounting the folder for {}. Try again when it finishes.",
                file.display()
            ));
        }
    }
    // The reservation may already have ended since the editor checked the path.
    if git::pinned(file).is_some() {
        return Err("The host editor is disabled for files in sandbox folders.".into());
    }
    let terminal = start()?;
    access.editors.retain(OpenEditor::is_open);
    access.editors.push(OpenEditor { file: file.to_owned(), terminal: Arc::downgrade(&terminal.shared) });
    Ok(terminal)
}

/// Keep the reservation alive until preparation finishes, including if its tile closes.
pub(crate) fn reserve(folders: impl IntoIterator<Item = PathBuf>) -> Result<Arc<[PathBuf]>, String> {
    let folders: Arc<[PathBuf]> = folders.into_iter().collect();
    let mut access = ACCESS.lock().unwrap();
    access.editors.retain(OpenEditor::is_open);
    if let Some(editor) =
        access.editors.iter().find(|editor| folders.iter().any(|folder| paths::at_or_below(&editor.file, folder)))
    {
        return Err(format!("Close the editor on {} first", editor.file.display()));
    }
    access.mounts.retain(|mount| mount.strong_count() != 0);
    access.mounts.push(Arc::downgrade(&folders));
    Ok(folders)
}

pub(super) fn reserve_writable(mounts: &[String]) -> anyhow::Result<Arc<[PathBuf]>> {
    reserve(mounts.iter().filter(|mount| !mount.ends_with(":ro")).map(PathBuf::from)).map_err(anyhow::Error::msg)
}
