use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use atomics_core::usage::Usage;

const POLL_INTERVAL: Duration = Duration::from_secs(60);
const TAIL_BYTES: u64 = 16 * 1024;

pub enum Report {
    Claude(Usage),
    Codex(Usage),
}

pub struct UsagePoller {
    pub(crate) claude: Usage,
    pub(crate) codex: Usage,
    reports: Receiver<Report>,
    stop: Sender<()>,
}

impl UsagePoller {
    pub fn new(window: crate::host::Window) -> (Self, Sender<Report>) {
        let (tx, reports) = channel();
        let (stop, stopped) = channel();
        let home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| atomics_core::paths::home().join(".codex"));
        let poll_tx = tx.clone();
        std::thread::Builder::new()
            .name("atomics-usage".into())
            .spawn(move || poll_codex(&home.join("sessions"), poll_tx, stopped, window, POLL_INTERVAL))
            .expect("usage worker");
        (Self { claude: Usage::default(), codex: Usage::default(), reports, stop }, tx)
    }

    pub fn drain(&mut self) -> bool {
        let mut changed = false;
        while let Ok(report) = self.reports.try_recv() {
            changed |= match report {
                Report::Claude(usage) => self.claude.update(usage),
                Report::Codex(usage) => self.codex.update(usage),
            };
        }
        changed
    }
}

impl Drop for UsagePoller {
    fn drop(&mut self) {
        let _ = self.stop.send(());
    }
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64
}

fn poll_codex(
    sessions: &Path,
    tx: Sender<Report>,
    stopped: Receiver<()>,
    window: crate::host::Window,
    interval: Duration,
) {
    let mut previous = None;
    loop {
        if let Some(usage) = read_codex(sessions)
            && previous.as_ref() != Some(&usage)
        {
            previous = Some(usage.clone());
            if tx.send(Report::Codex(usage)).is_err() {
                break;
            }
            window.request_repaint();
        }
        if stopped.recv_timeout(interval) != Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
            break;
        }
    }
}

/// Account usage read from running sandboxes, whose agents reach neither ATOMICS nor this machine's
/// Codex logs: one `sbx exec` per sandbox, at most once per [`POLL_INTERVAL`], off the UI thread.
pub struct SandboxPull {
    tx: Sender<Report>,
    pulled: Option<Instant>,
    busy: Arc<AtomicBool>,
}

impl SandboxPull {
    pub fn new(tx: Sender<Report>) -> Self {
        Self { tx, pulled: None, busy: Arc::default() }
    }

    /// `names` are sandboxes whose agent is attached: `sbx exec` would start a stopped one.
    pub fn poll(&mut self, sbx: &str, names: Vec<String>, window: &crate::host::Window) {
        if names.is_empty()
            || self.busy.load(Ordering::Acquire)
            || self.pulled.is_some_and(|at| at.elapsed() < POLL_INTERVAL)
        {
            return;
        }
        self.pulled = Some(Instant::now());
        self.busy.store(true, Ordering::Release);
        let (tx, busy, window, sbx) = (self.tx.clone(), self.busy.clone(), window.clone(), sbx.to_string());
        let spawned = std::thread::Builder::new().name("atomics-sandbox-usage".into()).spawn(move || {
            let sbx = crate::setup::resolve_program(&sbx);
            for name in names {
                let Ok(output) = crate::sandbox::read_usage(&sbx, &name) else { continue };
                for report in sandbox_reports(&output, now()) {
                    let _ = tx.send(report);
                }
            }
            busy.store(false, Ordering::Release);
            window.request_repaint();
        });
        if spawned.is_err() {
            self.busy.store(false, Ordering::Release);
        }
    }
}

/// Reports in [`atomics_core::isolation::usage_args`]' output. The sandbox's agent can write all of
/// it, so nothing may claim to be newer than now: that would hide every real report after it.
pub fn sandbox_reports(output: &str, now: i64) -> Vec<Report> {
    let mut parts: Vec<(&str, Vec<&str>)> = Vec::new();
    for line in output.lines() {
        match line.strip_prefix(atomics_core::isolation::USAGE_MARK) {
            Some(header) => parts.push((header, Vec::new())),
            None => {
                if let Some((_, lines)) = parts.last_mut() {
                    lines.push(line);
                }
            }
        }
    }
    let mut reports = Vec::new();
    for (header, lines) in parts {
        let mut words = header.split_whitespace();
        match words.next() {
            Some("codex") => {
                if let Some(mut usage) = lines.iter().rev().find_map(|line| Usage::codex(line)) {
                    usage.observed_at = usage.observed_at.min(now);
                    reports.push(Report::Codex(usage));
                }
            }
            Some("claude") => {
                let Some(at) = words.next().and_then(|word| word.parse::<i64>().ok()) else { continue };
                let Ok(payload) = serde_json::from_str::<serde_json::Value>(&lines.join("\n")) else { continue };
                let usage = Usage::claude(&payload, at.min(now));
                if usage.session.is_some() || usage.weekly.is_some() {
                    reports.push(Report::Claude(usage));
                }
            }
            _ => {}
        }
    }
    reports
}

fn read_codex(sessions: &Path) -> Option<Usage> {
    let path = newest_rollout(sessions)?;
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let offset = length.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(offset.saturating_sub(1))).ok()?;
    let mut preceding = *b"\n";
    if offset > 0 {
        file.read_exact(&mut preceding).ok()?;
    }
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    let tail = String::from_utf8_lossy(&bytes);
    let tail = if preceding[0] != b'\n' { tail.split_once('\n')?.1 } else { &tail };
    tail.lines().rev().find_map(Usage::codex)
}

fn newest_rollout(sessions: &Path) -> Option<PathBuf> {
    let mut dirs = vec![sessions.to_path_buf()];
    // Codex stores logs under YYYY/MM/DD. Ignore symlinks and unrelated files.
    for _ in 0..3 {
        dirs = dirs
            .iter()
            .filter_map(|dir| std::fs::read_dir(dir).ok())
            .flatten()
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.path())
            .collect();
    }
    dirs.iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter(|entry| {
            entry.file_name().to_str().is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
        })
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .max()
        .map(|(_, path)| path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn draining_reports_keeps_the_latest_account_values_and_ignores_duplicates() {
        let (tx, reports) = channel();
        let (stop, _) = channel();
        let mut poller = UsagePoller { claude: Usage::default(), codex: Usage::default(), reports, stop };
        let latest = Usage { observed_at: 100, ..Default::default() };
        tx.send(Report::Claude(latest.clone())).unwrap();
        tx.send(Report::Codex(latest.clone())).unwrap();
        assert!(poller.drain());
        tx.send(Report::Claude(Usage { observed_at: 99, ..Default::default() })).unwrap();
        tx.send(Report::Codex(latest.clone())).unwrap();
        assert!(!poller.drain());
        assert_eq!(poller.claude, latest);
        assert_eq!(poller.codex, latest);
    }

    #[test]
    fn poller_runs_off_thread_rechecks_and_does_not_repaint_unchanged_data() {
        let dir = crate::test_support::tempdir().unwrap();
        let day = dir.path().join("2026/10/28");
        std::fs::create_dir_all(&day).unwrap();
        let path = day.join("rollout-test.jsonl");
        let event = json!({"timestamp":"2026-10-28T15:00:00Z","type":"event_msg","payload":{
            "type":"token_count","rate_limits":{"primary":{"used_percent":15,"window_minutes":300,"resets_at":1793207640}}
        }});
        std::fs::write(&path, event.to_string()).unwrap();
        let ctx = egui::Context::default();
        let repaints = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = repaints.clone();
        ctx.set_request_repaint_callback(move |_| {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        // Clear egui's initial repaint before observing the worker.
        ctx.run_ui(Default::default(), |_| {}).drop_without_applying_deltas();
        ctx.run_ui(Default::default(), |_| {}).drop_without_applying_deltas();
        let (tx, rx) = channel();
        let (stop, stopped) = channel();
        let sessions = dir.path().to_path_buf();
        let worker = std::thread::spawn(move || {
            poll_codex(&sessions, tx, stopped, crate::host::Window::attach(ctx), Duration::from_millis(20))
        });
        assert!(matches!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), Report::Codex(_)));
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err(), "idle polls must not send data");
        let before = repaints.load(std::sync::atomic::Ordering::SeqCst);
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        assert_eq!(repaints.load(std::sync::atomic::Ordering::SeqCst), before);
        let mut changed = event;
        changed["payload"]["rate_limits"]["primary"]["used_percent"] = json!(25);
        std::fs::write(path, changed.to_string()).unwrap();
        let Report::Codex(usage) = rx.recv_timeout(Duration::from_secs(2)).unwrap() else { panic!("codex report") };
        assert_eq!(usage.session.unwrap().left_percent, 75.0);
        stop.send(()).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn sandbox_reports_read_both_agents_and_never_claim_to_be_newer_than_now() {
        use atomics_core::isolation::USAGE_MARK;
        let codex = r#"{"timestamp":"2026-10-30T11:32:20.000Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":12.0,"window_minutes":10080,"resets_at":1793864523}}}}"#;
        let claude = r#"{"model":{"id":"claude-sonnet-5-5"},"rate_limits":{"five_hour":{"used_percentage":40,"resets_at":1793370000},"seven_day":{"used_percentage":10,"resets_at":1793800000}}}"#;
        let now = 1_793_361_000;
        // tail -c starts mid-line; the newest complete event wins.
        let output = format!("{USAGE_MARK}codex\n{{\"cut off\n{codex}\n\n{USAGE_MARK}claude 1793360900\n{claude}\n");
        let reports = sandbox_reports(&output, now);
        let [Report::Codex(codex_usage), Report::Claude(claude_usage)] = reports.as_slice() else {
            panic!("a Codex report, then a Claude report");
        };
        assert_eq!((codex_usage.weekly.as_ref().unwrap().left_percent, codex_usage.observed_at), (88.0, 1_793_359_940));
        assert_eq!(claude_usage.session.as_ref().unwrap().left_percent, 60.0);
        assert_eq!(claude_usage.weekly.as_ref().unwrap().left_percent, 90.0);
        assert_eq!(claude_usage.observed_at, 1_793_360_900, "the status line file's time, not the read's");

        // The agent writes all of it: a future time would hide every real report after it.
        let future = format!(
            "{USAGE_MARK}codex\n{}\n{USAGE_MARK}claude 4102444800\n{claude}\n",
            codex.replace("2026-10-30", "2099-01-01")
        );
        let reports = sandbox_reports(&future, now);
        assert_eq!(reports.len(), 2);
        for report in &reports {
            let (Report::Codex(usage) | Report::Claude(usage)) = report;
            assert_eq!(usage.observed_at, now);
        }

        // A mark inside a line starts nothing; parts without limits, times or JSON report nothing.
        assert!(sandbox_reports(&format!("noise {USAGE_MARK}claude 5\n{claude}\n"), now).is_empty());
        let empty = format!(
            "{USAGE_MARK}claude 5\n{{\"model\":{{}}}}\n{USAGE_MARK}claude\n{claude}\n{USAGE_MARK}codex\nnot json\n"
        );
        assert!(sandbox_reports(&empty, now).is_empty());
    }

    #[test]
    fn log_reader_uses_newest_rollout_and_bounded_tail() {
        let dir = crate::test_support::tempdir().unwrap();
        let day = dir.path().join("2026/10/28");
        std::fs::create_dir_all(&day).unwrap();
        let event = json!({"timestamp":"2026-10-28T15:00:00Z","type":"event_msg","payload":{
            "type":"token_count","rate_limits":{"primary":{"used_percent":15,"window_minutes":300,"resets_at":1793207640}}
        }}).to_string();
        let old = day.join("rollout-old.jsonl");
        std::fs::write(&old, &event).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(1))
            .unwrap();
        let newest = day.join("rollout-new.jsonl");
        std::fs::write(&newest, format!("{event}\n{}\n{{\"partial\":", "x".repeat(TAIL_BYTES as usize))).unwrap();
        assert_eq!(newest_rollout(dir.path()).unwrap(), newest);
        assert!(read_codex(dir.path()).is_none(), "must not read an old event outside the tail");
        std::fs::write(&newest, format!("{}\n{event}\n{{\"partial\":", "x".repeat(TAIL_BYTES as usize))).unwrap();
        assert_eq!(read_codex(dir.path()).unwrap().session.unwrap().left_percent, 85.0);
        assert!(read_codex(&dir.path().join("missing")).is_none());
    }

    #[test]
    fn log_reader_keeps_a_complete_event_at_the_tail_boundary() {
        let dir = crate::test_support::tempdir().unwrap();
        let day = dir.path().join("2026/10/28");
        std::fs::create_dir_all(&day).unwrap();
        let event: serde_json::Value =
            serde_json::from_str(include_str!("../../atomics-core/tests/fixtures/codex-token-count-weekly-only.json"))
                .unwrap();
        let event = event.to_string();
        let tail = format!("{event}\n{}", " ".repeat(TAIL_BYTES as usize - event.len() - 1));
        assert_eq!(tail.len(), TAIL_BYTES as usize);
        let path = day.join("rollout-boundary.jsonl");
        std::fs::write(&path, format!("ignored prefix\n{tail}")).unwrap();
        assert_eq!(read_codex(dir.path()).unwrap().weekly.unwrap().left_percent, 90.0);
        std::fs::write(&path, format!("ignored prefix{tail}")).unwrap();
        assert!(read_codex(dir.path()).is_none(), "a fragment at the boundary is not an event");
    }
}
