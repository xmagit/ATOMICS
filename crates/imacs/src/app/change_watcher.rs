//! The platform file watcher, reporting changes only. On Linux, inotify also reports every file
//! opened, and closed without writing: each git command IMACS runs reads `.git/HEAD` and refs, and
//! would look like a ref change, which starts the integration checks again, whose git reads them again.

use std::path::Path;

use notify_debouncer_mini::notify::{
    self, Event, EventHandler, RecommendedWatcher, RecursiveMode, Watcher, WatcherKind,
};

pub(crate) struct ChangeWatcher(RecommendedWatcher);

/// Opening, reading or closing a file changes nothing a create, modify or remove event does not report.
fn changes(event: &notify::Result<Event>) -> bool {
    !event.as_ref().is_ok_and(|event| event.kind.is_access())
}

impl Watcher for ChangeWatcher {
    fn new<F: EventHandler>(mut handler: F, config: notify::Config) -> notify::Result<Self> {
        let forward = move |event: notify::Result<Event>| {
            if changes(&event) {
                handler.handle_event(event);
            }
        };
        RecommendedWatcher::new(forward, config).map(Self)
    }

    fn watch(&mut self, path: &Path, recursive_mode: RecursiveMode) -> notify::Result<()> {
        self.0.watch(path, recursive_mode)
    }

    fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        self.0.unwatch(path)
    }

    fn kind() -> WatcherKind {
        RecommendedWatcher::kind()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    #[test]
    fn reading_a_ref_is_no_change_and_writing_one_is() {
        let dir = crate::test_support::tempdir().unwrap();
        let head = dir.path().join("HEAD");
        std::fs::write(&head, "ref: refs/heads/main\n").unwrap();
        let (tx, rx) = channel();
        let mut watcher = ChangeWatcher::new(tx, notify::Config::default()).unwrap();
        watcher.watch(dir.path(), RecursiveMode::Recursive).unwrap();
        for _ in 0..3 {
            std::fs::read_to_string(&head).unwrap();
        }
        let quiet = Duration::from_millis(300);
        assert!(rx.recv_timeout(quiet).is_err(), "reading a file is not a change");
        std::fs::write(&head, "ref: refs/heads/other\n").unwrap();
        let event = rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(5))).unwrap().unwrap();
        assert!(event.paths.contains(&head), "{event:?}");
    }
}
