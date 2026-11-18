//! Host retention, including verified drops from before the receipt catalog existed.
use super::*;

#[derive(Serialize)]
pub struct StoredFile {
    pub id: String,
    name: String,
    size: u64,
    plan: Option<String>,
    path: PathBuf,
}

#[derive(Serialize)]
pub struct Storage {
    pub retained_bytes: u64,
    pub free_bytes: u64,
    pub files: Vec<StoredFile>,
    next: Option<String>,
}

pub(super) fn free_bytes(dir: &File) -> Result<u64, String> {
    let volume = fs::fstatvfs(dir).map_err(error)?;
    Ok(volume.f_bavail.saturating_mul(volume.f_frsize))
}

/// The verified manifest exists before a commit releases its reservation. owner.json also covers
/// legacy retained files. Inspect actual file sizes, including files changed by the same OS user.
fn visit(root: &File, mut file: impl FnMut(&str, &str, &str, u64, &File) -> Result<(), String>) -> Result<(), String> {
    for id in names(root)?.into_iter().filter(|id| drop_id(id)) {
        let dir = child(root, &id, true)?;
        if child(&dir, "owner.json", false).is_err() && child(&dir, "verified.json", false).is_err() {
            continue;
        }
        for index in names(&dir)?.into_iter().filter(|n| numbered(n)) {
            let folder = child(&dir, &index, true)?;
            for name in names(&folder)? {
                let size = child(&folder, &name, false)?.metadata().map_err(error)?.len();
                file(&id, &index, &name, size, &dir)?;
            }
        }
    }
    Ok(())
}

pub(super) fn retained(root: &File) -> Result<u64, String> {
    let mut bytes = 0u64;
    visit(root, |_, _, _, size, _| {
        bytes = bytes.saturating_add(FILE_OVERHEAD).saturating_add(size);
        Ok(())
    })?;
    Ok(bytes)
}

/// A bounded page sorted by id. The usage total covers the whole store on every page.
pub fn storage(home: &Path, after: Option<&str>) -> Result<Storage, String> {
    let root = private_root(home)?;
    let base = home.canonicalize().map_err(error)?.join("attachments");
    let mut bytes = 0u64;
    let mut page = std::collections::BTreeMap::new();
    visit(&root, |drop, index, name, size, dir| {
        bytes = bytes.saturating_add(FILE_OVERHEAD).saturating_add(size);
        let id = format!("{drop}-{index}");
        if after.is_some_and(|after| id.as_str() <= after)
            || page.len() == 101 && page.last_key_value().is_some_and(|(last, _)| last < &id)
        {
            return Ok(());
        }
        let Ok(plan) = read_json::<Option<String>>(dir, "owner.json") else { return Ok(()) };
        page.insert(
            id.clone(),
            StoredFile { id, name: name.into(), size, plan, path: base.join(drop).join(index).join(name) },
        );
        if page.len() > 101 {
            page.pop_last();
        }
        Ok(())
    })?;
    let next = if page.len() > 100 {
        page.pop_last();
        page.last_key_value().map(|(id, _)| id.clone())
    } else {
        None
    };
    Ok(Storage { retained_bytes: bytes, free_bytes: free_bytes(&root)?, files: page.into_values().collect(), next })
}

/// Only the owner's local control channel exposes deletion. It accepts a stored id, never a path.
pub fn delete(home: &Path, id: &str) -> Result<u64, String> {
    let (drop, index) =
        id.rsplit_once('-').filter(|(d, i)| drop_id(d) && numbered(i)).ok_or("Invalid attachment id")?;
    let root = private_root(home)?;
    let dir = child(&root, drop, true)?;
    read_json::<Option<String>>(&dir, "owner.json")?;
    let folder = child(&dir, index, true)?;
    let entries = names(&folder)?;
    let [name] = entries.as_slice() else { return Err("Invalid retained file directory".into()) };
    let size = child(&folder, name, false)?.metadata().map_err(error)?.len();
    fs::unlinkat(&folder, name.as_str(), AtFlags::empty()).map_err(error)?;
    fs::unlinkat(&dir, index, AtFlags::REMOVEDIR).map_err(error)?;
    if !names(&dir)?.iter().any(|n| numbered(n)) {
        // This drop has been committed; a new upload never reuses its id.
        std::fs::remove_dir_all(home.join("attachments").join(drop)).map_err(error)?;
    }
    Ok(size)
}
