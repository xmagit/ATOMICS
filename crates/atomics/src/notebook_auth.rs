//! Capabilities are minted by the host, retained across resume, and never accepted as tool arguments.
//! These files and bare processes share the owning OS user's trust domain; use sandboxes for isolation.
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const ENV: &str = "ATOMICS_NOTE_CAPABILITY";
pub const MODE_ENV: &str = "ATOMICS_NOTE_MODE";

fn path(home: &Path, pane: &str) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        !pane.is_empty() && pane.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
        "Invalid notebook session"
    );
    Ok(home.join("notebook-auth").join(pane))
}

pub fn read(home: &Path, pane: &str) -> Option<String> {
    let path = path(home, pane).ok()?;
    if !std::fs::symlink_metadata(&path).ok()?.is_file() {
        return None;
    }
    std::fs::read_to_string(path).ok().filter(|s| uuid::Uuid::parse_str(s).is_ok())
}

pub fn issue(home: &Path, pane: &str) -> anyhow::Result<String> {
    let path = path(home, pane)?;
    std::fs::create_dir_all(path.parent().expect("capability directory"))?;
    let token = uuid::Uuid::new_v4().to_string();
    match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
        Ok(mut file) => {
            file.write_all(token.as_bytes())?;
            file.sync_all()?;
            Ok(token)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            read(home, pane).ok_or_else(|| anyhow::anyhow!("Invalid notebook capability file"))
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notebook_capabilities_are_distinct_durable_and_do_not_follow_links() {
        let dir = crate::test_support::tempdir().unwrap();
        let a = issue(dir.path(), "a").unwrap();
        assert_eq!(a, issue(dir.path(), "a").unwrap());
        assert_ne!(a, issue(dir.path(), "b").unwrap());
        assert!(issue(dir.path(), "../a").is_err());
        std::os::unix::fs::symlink(dir.path().join("notebook-auth/a"), dir.path().join("notebook-auth/link")).unwrap();
        assert!(issue(dir.path(), "link").is_err());
    }
}
