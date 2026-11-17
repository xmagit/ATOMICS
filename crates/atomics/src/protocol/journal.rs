//! The operation journal. A mutation is recorded before it takes effect and its outcome once it
//! has one, so sending the same operation ID again returns what happened instead of doing it twice.
//!
//! An operation is known by who sent it (`desktop`; `client:` and a loopback client's ID; or
//! `device:`, a paired device's key digest and its client's ID) and its `op`; a paired device's
//! beginning also records its name (`by`). Its command's fingerprint is kept with it: the same ID with a different command is a
//! `conflict`, never the old outcome. With a file (while the protocol server runs), each record is
//! one JSON line, appended before the work starts and after it ends. If the host process dies in
//! between, the next start finds a beginning without an end and answers `uncertain` for it: the
//! host never guesses whether a message was typed or a merge ran. Each line is synced to disk
//! (`sync_data`) before the work starts and before the outcome is answered, so neither the process
//! dying nor the machine losing power can make the host forget an operation it began and run it
//! again. That is why a beginning is synced under the `State` lock: the operation must be durable
//! before it can run, so a retry after a power cut is answered, not run again. Mutations come from
//! people's clicks and are rare, so the sync costs little. An outcome is synced after the lock is
//! released (`take_sync`), before any client is answered with it. Compaction writes and syncs a new
//! file, renames it into place and syncs the directory.
//!
//! Outcomes are kept for `KEEP_SECS` from when the operation began (twice the `op_ttl_secs` that
//! clients are told), at most `CAPACITY` at once; an operation still running is never forgotten.
//! Beyond that, a new operation is refused rather than an unexpired one forgotten.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use atomics_core::ops::{Command, OP_TTL_SECS, Outcome};
use serde::{Deserialize, Serialize};

pub const KEEP_SECS: u64 = 2 * OP_TTL_SECS as u64;
pub const CAPACITY: usize = 4096;
/// A `done` result kept for a retry is cut to this size; the first answer carries it whole.
const KEPT_RESULT_BYTES: usize = 8 * 1024;

/// Who sent an operation, and its ID.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Key {
    pub origin: String,
    pub op: String,
}

impl Key {
    pub fn desktop(op: String) -> Self {
        Self { origin: "desktop".into(), op }
    }

    pub fn client(client_id: &str, op: &str) -> Self {
        Self { origin: format!("client:{client_id}"), op: op.into() }
    }

    /// From a paired device: `device_id` (`PeerDevice::id`) keeps it apart from other devices'
    /// clients with the same ID.
    pub fn device(device_id: &str, client_id: &str, op: &str) -> Self {
        Self { origin: format!("device:{device_id}:{client_id}"), op: op.into() }
    }
}

/// What `begin` found.
#[derive(Debug, Clone, PartialEq)]
pub enum Begun {
    /// New: recorded; carry it out and `finish` it.
    Fresh,
    /// Done before; this is how it ended.
    Known(Outcome),
    /// Still running in this process; its outcome follows.
    Running,
    /// The ID was used for a different command.
    Conflict,
    /// Too many operations are remembered to take another now.
    Full,
    /// The record could not be written, so the work was not started.
    Unwritable(String),
}

struct Entry {
    fingerprint: String,
    began: u64,
    outcome: Option<Outcome>,
    /// The paired device's name, when one sent it.
    by: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
enum Record {
    Begin {
        #[serde(flatten)]
        key: Key,
        fingerprint: String,
        at: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
    },
    End {
        #[serde(flatten)]
        key: Key,
        outcome: Outcome,
    },
}

struct Log {
    path: PathBuf,
    file: std::sync::Arc<File>,
    lines: usize,
}

pub struct Journal {
    entries: HashMap<Key, Entry>,
    log: Option<Log>,
    error: Option<String>,
    pending_sync: bool,
}

/// A command's fingerprint: FNV-1a over its JSON. It tells a reused ID apart from a retry; it is
/// not a security boundary (the ID's owner is the only one who could make the two collide).
pub fn fingerprint(command: &Command) -> String {
    let bytes = serde_json::to_vec(command).unwrap_or_default();
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3));
    format!("{hash:016x}")
}

fn kept(outcome: &Outcome) -> Outcome {
    match outcome {
        Outcome::Done { result } if atomics_core::wire::json_len(result) > KEPT_RESULT_BYTES => {
            Outcome::done(serde_json::json!({ "trimmed": true }))
        }
        other => other.clone(),
    }
}

fn private_file(path: &Path, append: bool) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).mode(0o600);
    if append {
        options.append(true);
    } else {
        options.write(true).truncate(true);
    }
    options.open(path)
}

impl Journal {
    /// A journal with no file: the desktop's own operations while the protocol server is off.
    pub fn memory() -> Self {
        Self { entries: HashMap::new(), log: None, error: None, pending_sync: false }
    }

    pub fn unwritable(error: String) -> Self {
        Self { error: Some(error), ..Self::memory() }
    }

    /// Load the journal at `path` (mode 0600). An operation that began without ending is
    /// `uncertain` from now on, and forgotten ones are left out of the file written back.
    pub fn open(path: &Path, now: u64) -> io::Result<Self> {
        let mut entries: HashMap<Key, Entry> = HashMap::new();
        match File::open(path) {
            Ok(file) => {
                for line in BufReader::new(file).split(b'\n') {
                    // A line cut short by a crash (even inside a character) is skipped like any
                    // other unreadable one.
                    let Ok(record) = serde_json::from_slice::<Record>(&line?) else { continue };
                    match record {
                        Record::Begin { key, fingerprint, at, by } => {
                            entries.insert(key, Entry { fingerprint, began: at, outcome: None, by });
                        }
                        Record::End { key, outcome } => {
                            if let Some(entry) = entries.get_mut(&key) {
                                entry.outcome = Some(outcome);
                            }
                        }
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        for entry in entries.values_mut() {
            entry.outcome.get_or_insert_with(|| Outcome::Uncertain {
                message: "the host stopped while this ran; check the board before trying again".into(),
            });
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Remove a torn final record before any append. Compaction below also drops
        // malformed complete lines and preserves valid legacy records without a newline.
        if let Ok(bytes) = std::fs::read(path)
            && !bytes.ends_with(b"\n")
        {
            let tail = bytes.rsplit(|b| *b == b'\n').next().unwrap_or_default();
            if serde_json::from_slice::<Record>(tail).is_err() {
                let len = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
                std::fs::OpenOptions::new().write(true).open(path)?.set_len(len as u64)?;
            }
        }
        // Opened first only so that `compact` has a log to replace.
        let file = private_file(path, true)?;
        let mut journal = Self {
            entries,
            log: Some(Log { path: path.to_path_buf(), file: std::sync::Arc::new(file), lines: 0 }),
            error: None,
            pending_sync: false,
        };
        journal.expire(now);
        journal.compact()?;
        Ok(journal)
    }

    /// Why the journal cannot be written, once it could not: kept until the host restarts (no
    /// record may follow one that failed), so new client mutations are refused meanwhile. The
    /// desktop's Remote devices and `atomics ctl '{"cmd":"remote"}'` show it.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Operations remembered, for tests and diagnostics.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Look the operation up, or record its beginning (with `by`, the name of the paired device
    /// that sent it). Only `Fresh` means "carry it out".
    pub fn begin(&mut self, key: &Key, by: Option<&str>, fingerprint: &str, now: u64) -> Begun {
        self.expire(now);
        if let Some(entry) = self.entries.get(key) {
            if entry.fingerprint != fingerprint {
                return Begun::Conflict;
            }
            return entry.outcome.clone().map_or(Begun::Running, Begun::Known);
        }
        if self.entries.len() >= CAPACITY && key.origin != "desktop" {
            return Begun::Full;
        }
        let by = by.map(str::to_string);
        let record = Record::Begin { key: key.clone(), fingerprint: fingerprint.into(), at: now, by: by.clone() };
        // Synced now, under the `State` lock: the operation must be durable before it can run, so a
        // retry after a power cut is answered and not run again.
        if let Err(e) = self.append(&record) {
            self.error = Some(e.to_string());
            return Begun::Unwritable(e.to_string());
        }
        self.entries.insert(key.clone(), Entry { fingerprint: fingerprint.into(), began: now, outcome: None, by });
        Begun::Fresh
    }

    /// Record how the operation ended, on disk before the caller answers with it. A failed write
    /// is reported and the outcome is still kept in memory; after a crash the operation would then
    /// read as `uncertain`, which is true.
    #[cfg(test)]
    pub fn finish(&mut self, key: &Key, outcome: &Outcome) {
        self.finish_deferred(key, outcome);
        if let Some(file) = self.take_sync() {
            let _ = file.sync_data();
        }
    }

    /// Append the outcome while State is locked; the runtime syncs it before replying,
    /// after releasing State. Begin records still sync before side effects.
    pub fn finish_deferred(&mut self, key: &Key, outcome: &Outcome) {
        let Some(entry) = self.entries.get_mut(key) else { return };
        let outcome = kept(outcome);
        entry.outcome = Some(outcome.clone());
        if let Err(e) = self.append_inner(&Record::End { key: key.clone(), outcome }, false) {
            self.error = Some(e.to_string());
            eprintln!("atomics: the operation journal could not record an outcome: {e}");
        }
        if self.log.as_ref().is_some_and(|log| log.lines > 2 * self.entries.len() + 64)
            && let Err(e) = self.compact()
        {
            eprintln!("atomics: the operation journal could not be compacted: {e}");
        }
    }

    fn expire(&mut self, now: u64) {
        self.entries.retain(|_, e| e.outcome.is_none() || now.saturating_sub(e.began) < KEEP_SECS);
    }

    pub fn take_sync(&mut self) -> Option<std::sync::Arc<File>> {
        if !std::mem::take(&mut self.pending_sync) {
            return None;
        }
        self.log.as_ref().map(|log| log.file.clone())
    }

    fn append(&mut self, record: &Record) -> io::Result<()> {
        self.append_inner(record, true)
    }

    fn append_inner(&mut self, record: &Record, sync: bool) -> io::Result<()> {
        if let Some(error) = &self.error {
            return Err(io::Error::other(error.clone()));
        }
        let Some(log) = &mut self.log else { return Ok(()) };
        let mut line = serde_json::to_vec(record).map_err(io::Error::other)?;
        line.push(b'\n');
        // One write per line, so a crash leaves at most the last line cut short; synced, so a power
        // cut cannot lose a beginning whose work went ahead.
        log.file.as_ref().write_all(&line)?;
        if sync {
            log.file.sync_data()?;
        } else {
            self.pending_sync = true;
        }
        log.lines += 1;
        Ok(())
    }

    /// Write what is remembered to a new file and put it in place of the log.
    fn compact(&mut self) -> io::Result<()> {
        self.compact_with(|path| private_file(path, true))
    }

    fn compact_with(&mut self, reopen: impl FnOnce(&Path) -> io::Result<File>) -> io::Result<()> {
        let Some(log) = &mut self.log else { return Ok(()) };
        let tmp = log.path.with_extension("jsonl.tmp");
        let mut out = Vec::new();
        let mut lines = 0;
        for (key, entry) in &self.entries {
            let begin = Record::Begin {
                key: key.clone(),
                fingerprint: entry.fingerprint.clone(),
                at: entry.began,
                by: entry.by.clone(),
            };
            serde_json::to_writer(&mut out, &begin).map_err(io::Error::other)?;
            out.push(b'\n');
            lines += 1;
            if let Some(outcome) = &entry.outcome {
                serde_json::to_writer(&mut out, &Record::End { key: key.clone(), outcome: outcome.clone() })
                    .map_err(io::Error::other)?;
                out.push(b'\n');
                lines += 1;
            }
        }
        let mut file = private_file(&tmp, false)?;
        file.write_all(&out)?;
        // On disk before it replaces the log, and the rename on disk before the log is written to.
        file.sync_data()?;
        drop(file);
        std::fs::rename(&tmp, &log.path)?;
        let dir = log.path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let synced = File::open(dir).and_then(|file| file.sync_all());
        // Once renamed, never keep an append handle to the old inode, even if syncing
        // the directory or reopening the new file fails.
        let reopened = reopen(&log.path);
        match reopened {
            Ok(file) => log.file = std::sync::Arc::new(file),
            Err(error) => {
                self.error = Some(error.to_string());
                self.log = None;
                return Err(error);
            }
        }
        log.lines = lines;
        synced?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomics_core::ops::{SessionAction, SessionCommand};
    use atomics_core::wire::ErrorCode;

    fn steer(op: &str, text: &str) -> Command {
        Command::SessionCommand(SessionCommand {
            op: op.into(),
            action: SessionAction::Steer { session: "a".into(), text: text.into(), now: false },
        })
    }

    fn begin(journal: &mut Journal, command: &Command, now: u64) -> Begun {
        journal.begin(&Key::client("phone", command.op()), None, &fingerprint(command), now)
    }

    #[test]
    fn failed_reopen_disables_writes_and_drops_the_stale_handle() {
        let dir = crate::test_support::tempdir().unwrap();
        let path = dir.path().join("ops.jsonl");
        let mut journal = Journal::open(&path, 0).unwrap();
        assert_eq!(begin(&mut journal, &steer("old", "x"), 0), Begun::Fresh);
        assert_eq!(journal.error(), None);
        assert!(journal.compact_with(|_| Err(io::Error::other("injected reopen failure"))).is_err());
        assert!(journal.log.is_none());
        assert_eq!(journal.error(), Some("injected reopen failure"), "kept, for the desktop to show");
        let before = std::fs::read(&path).unwrap();
        assert!(matches!(begin(&mut journal, &steer("new", "x"), 0), Begun::Unwritable(_)));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn torn_tail_is_removed_before_new_records_are_appended() {
        let dir = crate::test_support::tempdir().unwrap();
        let path = dir.path().join("ops.jsonl");
        let mut original = Journal::open(&path, 0).unwrap();
        assert_eq!(begin(&mut original, &steer("old", "x"), 0), Begun::Fresh);
        drop(original);
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{\"record\":\"beg").unwrap();
        let mut journal = Journal::open(&path, 0).unwrap();
        assert!(matches!(begin(&mut journal, &steer("old", "x"), 0), Begun::Known(Outcome::Uncertain { .. })));
        assert_eq!(begin(&mut journal, &steer("new", "x"), 0), Begun::Fresh);
        drop(journal);
        assert_eq!(Journal::open(&path, 0).unwrap().len(), 2);
    }

    #[test]
    fn outcome_sync_can_be_taken_and_run_without_the_journal_lock() {
        let dir = crate::test_support::tempdir().unwrap();
        let mut journal = Journal::open(&dir.path().join("ops.jsonl"), 0).unwrap();
        assert_eq!(begin(&mut journal, &steer("new", "x"), 0), Begun::Fresh);
        assert!(journal.take_sync().is_none(), "begin was already synced");
        journal.finish_deferred(&Key::client("phone", "new"), &Outcome::done(serde_json::Value::Null));
        let barrier = journal.take_sync().expect("outcome needs sync outside State");
        assert!(journal.take_sync().is_none());
        drop(journal);
        barrier.sync_data().unwrap();
    }

    #[test]
    fn a_retry_returns_the_known_outcome_and_a_reused_id_is_a_conflict() {
        let mut journal = Journal::memory();
        let command = steer("o1", "go on");
        assert_eq!(begin(&mut journal, &command, 10), Begun::Fresh);
        assert_eq!(begin(&mut journal, &command, 11), Begun::Running, "a retry while it runs waits for it");
        let done = Outcome::done(serde_json::json!({"sent": true}));
        journal.finish(&Key::client("phone", "o1"), &done);
        assert_eq!(begin(&mut journal, &command, 12), Begun::Known(done));
        assert_eq!(begin(&mut journal, &steer("o1", "something else"), 12), Begun::Conflict);
        // Another client's operation with the same ID is its own.
        let other = journal.begin(&Key::client("laptop", "o1"), None, &fingerprint(&command), 12);
        assert_eq!(other, Begun::Fresh);
    }

    #[test]
    fn a_paired_device_is_named_in_the_file_and_kept_apart_from_other_devices() {
        let dir = crate::test_support::tempdir().unwrap();
        let path = dir.path().join("ops.jsonl");
        let command = steer("o1", "go on");
        let phone = Key::device("0123456789abcdef", "browser", "o1");
        assert_eq!(phone.origin, "device:0123456789abcdef:browser");
        {
            let mut journal = Journal::open(&path, 10).unwrap();
            assert_eq!(journal.begin(&phone, Some("Test phone"), &fingerprint(&command), 10), Begun::Fresh);
            // Another device whose client chose the same ID: its own operation, not a retry.
            let tablet = Key::device("fedcba9876543210", "browser", "o1");
            assert_eq!(journal.begin(&tablet, Some("Tablet"), &fingerprint(&command), 10), Begun::Fresh);
            assert_eq!(journal.begin(&Key::client("browser", "o1"), None, &fingerprint(&command), 10), Begun::Fresh);
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""by":"Test phone""#) && text.contains(r#""by":"Tablet""#), "{text}");
        assert_eq!(text.matches(r#""by""#).count(), 2, "only devices are named: {text}");
        // Kept through a restart, which compacts the file.
        drop(Journal::open(&path, 11).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains(r#""by":"Test phone""#) && text.contains(r#""origin":"device:0123456789abcdef:browser""#),
            "{text}"
        );
        // A line written before devices were named still reads.
        std::fs::write(&path, r#"{"record":"begin","origin":"client:old","op":"o9","fingerprint":"x","at":11}"#)
            .unwrap();
        assert_eq!(Journal::open(&path, 12).unwrap().len(), 1);
    }

    #[test]
    fn an_operation_the_host_died_during_is_uncertain_after_a_restart() {
        let dir = crate::test_support::tempdir().unwrap();
        let path = dir.path().join("ops.jsonl");
        let (ran, crashed) = (steer("ran", "a"), steer("crashed", "b"));
        let refused = Outcome::Refused { code: ErrorCode::NotFound, message: "no such session".into() };
        {
            let mut journal = Journal::open(&path, 100).unwrap();
            assert_eq!(begin(&mut journal, &ran, 100), Begun::Fresh);
            journal.finish(&Key::client("phone", "ran"), &refused);
            assert_eq!(begin(&mut journal, &crashed, 101), Begun::Fresh);
            // The process dies here: `crashed` never finishes.
        }
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let mut journal = Journal::open(&path, 200).unwrap();
        assert_eq!(begin(&mut journal, &ran, 200), Begun::Known(refused), "a finished one is known across restarts");
        assert!(
            matches!(begin(&mut journal, &crashed, 200), Begun::Known(Outcome::Uncertain { .. })),
            "never run again, never guessed"
        );
        // A crash in the middle of writing a line loses only that line.
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(br#"{"record":"end","origin":"client:phone","op":"cra"#).unwrap();
        drop(file);
        let mut journal = Journal::open(&path, 300).unwrap();
        assert!(matches!(begin(&mut journal, &crashed, 300), Begun::Known(Outcome::Uncertain { .. })));
    }

    #[test]
    fn outcomes_are_kept_for_twice_the_client_ttl_and_running_ones_always() {
        let mut journal = Journal::memory();
        let (finished, running) = (steer("finished", "a"), steer("running", "b"));
        begin(&mut journal, &finished, 0);
        journal.finish(&Key::client("phone", "finished"), &Outcome::done(serde_json::Value::Null));
        begin(&mut journal, &running, 0);
        let later = KEEP_SECS - 1;
        assert!(matches!(begin(&mut journal, &finished, later), Begun::Known(_)));
        assert_eq!(begin(&mut journal, &finished, KEEP_SECS), Begun::Fresh, "forgotten after KEEP_SECS");
        assert_eq!(begin(&mut journal, &running, 10 * KEEP_SECS), Begun::Running);
    }

    #[test]
    fn a_full_journal_refuses_new_work_rather_than_forget_what_it_owes() {
        let mut journal = Journal::memory();
        for i in 0..CAPACITY {
            assert_eq!(begin(&mut journal, &steer(&i.to_string(), "x"), 0), Begun::Fresh);
        }
        assert_eq!(begin(&mut journal, &steer("one more", "x"), 1), Begun::Full);
        assert_eq!(begin(&mut journal, &steer("0", "x"), 1), Begun::Running, "known ones still answer");
    }

    #[test]
    fn the_file_is_compacted_and_a_large_result_is_kept_trimmed() {
        let dir = crate::test_support::tempdir().unwrap();
        let path = dir.path().join("ops.jsonl");
        let mut journal = Journal::open(&path, 0).unwrap();
        let big = Outcome::done(serde_json::json!({ "text": "x".repeat(KEPT_RESULT_BYTES) }));
        let run = |journal: &mut Journal, ops: std::ops::Range<usize>, now: u64| {
            for i in ops {
                let command = steer(&i.to_string(), "x");
                assert_eq!(begin(journal, &command, now), Begun::Fresh);
                journal.finish(&Key::client("phone", command.op()), &big);
            }
        };
        run(&mut journal, 0..200, 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 400);
        // Later those are forgotten, and the next operation's end rewrites the file without them.
        run(&mut journal, 200..210, KEEP_SECS);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 20, "two lines per remembered operation");
        let mut reopened = Journal::open(&path, KEEP_SECS).unwrap();
        assert_eq!(reopened.len(), 10);
        assert_eq!(
            begin(&mut reopened, &steer("205", "x"), KEEP_SECS),
            Begun::Known(Outcome::done(serde_json::json!({ "trimmed": true })))
        );
    }
}
