//! The browser origins this window should restore for one connection target.
use std::path::{Path, PathBuf};

use atomics_core::preview::Preview;
use serde::{Deserialize, Serialize};

const WAIT_SECONDS: u64 = 10 * 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Choice {
    #[serde(alias = "session")]
    pub source: Option<atomics_core::preview::PreviewSource>,
    pub preview: Preview,
    pub local: u16,
    pub tls: Option<bool>,
    /// Wall time so closing the window while waiting does not restart the deadline.
    pub waiting_since: Option<u64>,
    #[serde(skip)]
    pub next_attempt: u64,
    #[serde(skip)]
    pub launch_browser: bool,
}

impl Choice {
    pub fn paused(&self, now: u64) -> bool {
        self.waiting_since.is_some_and(|since| now.saturating_sub(since) >= WAIT_SECONDS)
    }
}

/// A file per remote port keeps unrelated opens and stops independent, including across windows.
pub(super) struct Store(pub PathBuf);

impl Store {
    pub fn load(&self) -> std::io::Result<Vec<Choice>> {
        let dir = match self.0.read_dir() {
            Ok(dir) => dir,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut choices = Vec::new();
        for entry in dir {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") || !entry.file_type()?.is_file() {
                continue;
            }
            let mut choice: Choice = serde_json::from_slice(&std::fs::read(&path)?)?;
            if self.file(choice.preview.port) == path && choice.preview.validate().is_ok() && choice.local > 0 {
                let safe = choice.preview.for_storage();
                if safe != choice.preview {
                    choice.preview = safe;
                    self.save(&choice)?;
                }
                choices.push(choice);
                if choices.len() == atomics_core::wire::MAX_TUNNELS {
                    break;
                }
            }
        }
        Ok(choices)
    }

    fn file(&self, port: u16) -> PathBuf {
        self.0.join(format!("{port}.json"))
    }

    pub fn save(&self, choice: &Choice) -> std::io::Result<()> {
        let saved = Choice { preview: choice.preview.for_storage(), ..choice.clone() };
        atomics_core::store::write_private(&self.file(choice.preview.port), &serde_json::to_vec(&saved)?)
    }

    pub fn forget(&self, port: u16) -> std::io::Result<()> {
        match std::fs::remove_file(self.file(port)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

pub(crate) fn directory(home: &Path, target: &crate::client::target::Target, instance: Option<&str>) -> PathBuf {
    use crate::client::target::Target;
    let scope = match target {
        Target::Local => "local".to_owned(),
        Target::Address { addr } => format!("address-{addr}"),
        Target::Vm => {
            // Instance names come from the bundle. Encode the component even for a hand-edited bundle.
            let name = instance.unwrap_or("atomics");
            let encoded: String = name.bytes().map(|b| format!("{b:02x}")).collect();
            format!("vm-{encoded}")
        }
    };
    home.join("client-previews").join(scope)
}
