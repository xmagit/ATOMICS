//! `ATOMICS_FRAME_STATS=<file>`: how often the window draws and how long each frame takes, appended to that
//! file every five seconds as CSV. Off unless the variable names a file; for measuring how smoothly a client
//! draws, not for everyday use.

use std::io::Write;
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(5);
const HEADER: &str =
    "seconds,frames,fps,interval_p50_ms,interval_p95_ms,interval_max_ms,work_p50_ms,work_p95_ms,work_max_ms";

pub(crate) struct FrameStats {
    file: std::fs::File,
    started: Instant,
    window_start: Instant,
    frame_start: Option<Instant>,
    /// Milliseconds between the starts of consecutive frames in this window.
    intervals: Vec<f32>,
    /// Milliseconds each frame spent in `logic` and `ui`.
    work: Vec<f32>,
}

impl FrameStats {
    pub(crate) fn from_env() -> Option<Self> {
        let path = std::env::var_os("ATOMICS_FRAME_STATS").filter(|path| !path.is_empty())?;
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).ok()?;
        writeln!(file, "{HEADER}").ok()?;
        let now = Instant::now();
        Some(Self { file, started: now, window_start: now, frame_start: None, intervals: Vec::new(), work: Vec::new() })
    }

    /// A frame starts: the first thing `logic` does.
    pub(crate) fn begin(&mut self, now: Instant) {
        if let Some(previous) = self.frame_start {
            self.intervals.push(millis(now - previous));
        }
        self.frame_start = Some(now);
        if now - self.window_start >= WINDOW {
            let line = summary(now - self.started, now - self.window_start, &mut self.intervals, &mut self.work);
            // A full disk or a removed file ends nothing; the measurement just stops being written.
            let _ = writeln!(self.file, "{line}");
            self.intervals.clear();
            self.work.clear();
            self.window_start = now;
        }
    }

    /// The frame's drawing is done: the last thing `ui` does.
    pub(crate) fn end(&mut self, now: Instant) {
        if let Some(start) = self.frame_start {
            self.work.push(millis(now - start));
        }
    }
}

fn millis(duration: Duration) -> f32 {
    duration.as_secs_f32() * 1000.0
}

/// One CSV line for a window of `span` that ended `elapsed` after the start.
fn summary(elapsed: Duration, span: Duration, intervals: &mut [f32], work: &mut [f32]) -> String {
    intervals.sort_by(f32::total_cmp);
    work.sort_by(f32::total_cmp);
    let frames = work.len();
    format!(
        "{:.0},{frames},{:.1},{:.1},{:.1},{:.1},{:.2},{:.2},{:.2}",
        elapsed.as_secs_f32(),
        frames as f32 / span.as_secs_f32().max(f32::EPSILON),
        percentile(intervals, 0.50),
        percentile(intervals, 0.95),
        intervals.last().copied().unwrap_or(0.0),
        percentile(work, 0.50),
        percentile(work, 0.95),
        work.last().copied().unwrap_or(0.0),
    )
}

/// The value at fraction `p` of an ascending slice; 0 for an empty one.
fn percentile(sorted: &[f32], p: f32) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f32 * p).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_of_an_ascending_slice() {
        let values: Vec<f32> = (1..=100).map(|v| v as f32).collect();
        assert_eq!(percentile(&values, 0.50), 51.0);
        assert_eq!(percentile(&values, 0.95), 95.0);
        assert_eq!(percentile(&[], 0.95), 0.0);
    }

    #[test]
    fn a_summary_counts_frames_per_second_over_its_window() {
        let mut intervals = vec![20.0, 16.0, 17.0];
        let mut work = vec![2.0, 1.0, 3.0, 1.5];
        let line = summary(Duration::from_secs(10), Duration::from_secs(2), &mut intervals, &mut work);
        assert_eq!(line, "10,4,2.0,17.0,20.0,20.0,2.00,3.00,3.00");
    }
}
