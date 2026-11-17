//! The working line: what an agent's own status line says while it works (`atomics_core::status_line`), as the
//! simple view shows it under the chief's conversation and on a task's row. The host sends a reading when its
//! words change and its numbers at most every two seconds; between readings this client counts on by itself: the
//! timer ticks from when the reading arrived, a new verb fades in, and the token count climbs to its new value.
//! Where the status line says nothing, the turn's start counts instead ("Working… 12s"). Under reduced motion
//! nothing moves but the numbers.

use std::collections::HashMap;

use atomics_core::status_line::LiveLine;
use egui::{Align2, Color32, FontId, Pos2, vec2};

/// How long a new verb takes to fade in, in seconds.
const FADE: f64 = 0.45;
/// How long the token count takes to climb to a new reading, in seconds.
const COUNT: f64 = 0.8;
/// How often the line redraws while only its timer moves; the spinner turns in steps on the same beat.
pub const TICK: std::time::Duration = std::time::Duration::from_millis(125);

/// One session's last reading and when this client got it, by its own clock (egui's input time).
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    line: LiveLine,
    at: f64,
    /// When the verb on screen first showed.
    verb_at: f64,
    /// The count the climb starts from, and when it started.
    tokens_from: u64,
    tokens_at: f64,
}

/// Each working session's reading, as this client counts on from it.
#[derive(Debug, Clone, Default)]
pub struct Working {
    seen: HashMap<String, Seen>,
}

/// What the working line shows this frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Shown {
    /// The agent's verb ("Fermenting"), or "Working" where its status line says nothing.
    pub verb: String,
    /// How far the verb has faded in, 0 to 1.
    pub verb_alpha: f32,
    /// How long the turn has run, in seconds, ticking on.
    pub elapsed: Option<u64>,
    /// The tokens, climbing to the last reading.
    pub tokens: Option<u64>,
    /// How long it thought, in seconds; zero while it thinks without saying.
    pub thought: Option<u64>,
    /// The words come from the agent's own line, not the turn's start alone.
    pub from_agent: bool,
}

impl Working {
    /// What to show for `session` at `now` (this client's clock, in seconds): from its `live` reading, else from
    /// `since` (the turn's start, host seconds) against `host_now`. A session without a reading is
    /// forgotten.
    pub fn shown(
        &mut self,
        session: &str,
        live: Option<&LiveLine>,
        since: Option<u64>,
        host_now: u64,
        now: f64,
        reduce_motion: bool,
    ) -> Option<Shown> {
        let Some(line) = live else {
            self.seen.remove(session);
            // A turn's start the clocks disagree on by more than a day says nothing.
            let elapsed = since.map(|since| host_now.saturating_sub(since)).filter(|&e| e < 86_400);
            return Some(Shown {
                verb: "Working".into(),
                verb_alpha: 1.0,
                elapsed,
                tokens: None,
                thought: None,
                from_agent: false,
            });
        };
        let seen = self.seen.entry(session.to_string()).or_insert_with(|| Seen {
            line: line.clone(),
            at: now,
            verb_at: now - FADE,
            tokens_from: line.tokens.unwrap_or(0),
            tokens_at: now - COUNT,
        });
        if seen.line != *line {
            if seen.line.verb != line.verb {
                seen.verb_at = now;
            }
            if seen.line.tokens != line.tokens {
                seen.tokens_from = counted(seen, now);
                seen.tokens_at = now;
            }
            seen.line = line.clone();
            seen.at = now;
        }
        let ticked = (now - seen.at).max(0.0) as u64;
        let verb_alpha = if reduce_motion { 1.0 } else { ((now - seen.verb_at) / FADE).clamp(0.0, 1.0) as f32 };
        let tokens = if reduce_motion { line.tokens } else { line.tokens.map(|_| counted(seen, now)) };
        Some(Shown {
            verb: line.verb.clone(),
            verb_alpha,
            elapsed: line.elapsed.map(|elapsed| elapsed + ticked),
            tokens,
            thought: line.thought,
            from_agent: true,
        })
    }

    /// The token count still climbing, at `now`.
    pub fn climbing(&self, now: f64) -> bool {
        self.seen.values().any(|seen| now - seen.tokens_at < COUNT || now - seen.verb_at < FADE)
    }
}

/// The count on its way from where the climb began to the reading, eased.
fn counted(seen: &Seen, now: f64) -> u64 {
    let to = seen.line.tokens.unwrap_or(0) as f64;
    let from = seen.tokens_from as f64;
    let t = ((now - seen.tokens_at) / COUNT).clamp(0.0, 1.0);
    let eased = 1.0 - (1.0 - t).powi(3);
    (from + (to - from) * eased).round() as u64
}

/// `14s`, `1m 23s`, `1h 21m`.
pub fn duration(seconds: u64) -> String {
    match seconds {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{}h {}m", s / 3600, s % 3600 / 60),
    }
}

/// `840`, `1.1k`, `12k`, `1.2M`.
pub fn tokens(count: u64) -> String {
    match count {
        c if c < 1000 => c.to_string(),
        c if c < 10_000 => format!("{:.1}k", c as f64 / 1000.0),
        c if c < 1_000_000 => format!("{}k", c / 1000),
        c => format!("{:.1}M", c as f64 / 1_000_000.0),
    }
}

impl Shown {
    /// The numbers after the verb: "14s · ↓ 1.1k tokens · thought for 1s".
    pub fn details(&self) -> String {
        let mut parts = Vec::new();
        if let Some(elapsed) = self.elapsed {
            parts.push(duration(elapsed));
        }
        if let Some(count) = self.tokens {
            parts.push(format!("↓ {} tokens", tokens(count)));
        }
        match self.thought {
            Some(0) => parts.push("thinking".into()),
            Some(seconds) => parts.push(format!("thought for {}", duration(seconds))),
            None => {}
        }
        parts.join(" · ")
    }

    /// The row's short form: "Fermenting… 14s · 1.1k".
    pub fn compact(&self) -> String {
        let mut text = format!("{}…", self.verb);
        if let Some(elapsed) = self.elapsed {
            text.push_str(&format!(" {}", duration(elapsed)));
        }
        if let Some(count) = self.tokens {
            text.push_str(&format!(" · {}", tokens(count)));
        }
        text
    }
}

/// The spinner: eight dots around a ring, one bright and its trail fading, stepping round on [`TICK`]'s beat; under
/// reduced motion, still.
pub fn spinner(painter: &egui::Painter, c: Pos2, r: f32, color: Color32, time: f64, reduce_motion: bool) {
    let step = if reduce_motion { 0 } else { (time / TICK.as_secs_f64()) as usize % 8 };
    for k in 0..8 {
        let angle = k as f32 / 8.0 * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
        let behind = (step + 8 - k) % 8;
        let alpha = if reduce_motion { 0.55 } else { 1.0 - behind as f32 * 0.11 };
        let dot = c + vec2(angle.cos(), angle.sin()) * r;
        painter.circle_filled(dot, 1.6, color.gamma_multiply(alpha.max(0.15)));
    }
}

/// The working line under the chief's conversation: the spinner, the verb fading in and its numbers.
pub fn line(ui: &mut egui::Ui, shown: &Shown, font: FontId, color: Color32, muted: Color32, time: f64, reduce: bool) {
    let height = font.size + 6.0;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), egui::Sense::hover());
    let painter = ui.painter();
    let c = rect.left_center() + vec2(8.0, 0.0);
    spinner(painter, c, 5.5, color, time, reduce);
    let verb = format!("{}…", shown.verb);
    let at = painter.text(
        c + vec2(14.0, 0.0),
        Align2::LEFT_CENTER,
        verb,
        font.clone(),
        color.gamma_multiply(0.25 + 0.75 * shown.verb_alpha),
    );
    let details = shown.details();
    if !details.is_empty() {
        painter.text(at.right_center() + vec2(8.0, 0.0), Align2::LEFT_CENTER, details, font, muted);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(verb: &str, elapsed: u64, tokens: u64) -> LiveLine {
        LiveLine { verb: verb.into(), elapsed: Some(elapsed), tokens: Some(tokens), thought: Some(1) }
    }

    #[test]
    fn the_timer_ticks_on_from_a_reading_and_the_tokens_climb_to_the_next() {
        let mut working = Working::default();
        let first = reading("Fermenting", 14, 1100);
        let shown = working.shown("s1", Some(&first), None, 0, 10.0, false).unwrap();
        assert_eq!((shown.elapsed, shown.tokens, shown.verb_alpha), (Some(14), Some(1100), 1.0));
        assert_eq!(shown.details(), "14s · ↓ 1.1k tokens · thought for 1s");
        let later = working.shown("s1", Some(&first), None, 0, 13.4, false).unwrap();
        assert_eq!(later.elapsed, Some(17), "three seconds on, with no new reading");
        // A new count climbs to it rather than jumping.
        let next = reading("Fermenting", 18, 2100);
        let climbing = working.shown("s1", Some(&next), None, 0, 14.0, false).unwrap();
        assert_eq!(climbing.tokens, Some(1100));
        assert!(working.climbing(14.2));
        let half = working.shown("s1", Some(&next), None, 0, 14.3, false).unwrap().tokens.unwrap();
        assert!(1100 < half && half < 2100, "{half}");
        assert_eq!(working.shown("s1", Some(&next), None, 0, 15.0, false).unwrap().tokens, Some(2100));
        assert!(!working.climbing(15.0));
        // A new verb fades in.
        let pondering = reading("Pondering", 19, 2100);
        let fading = working.shown("s1", Some(&pondering), None, 0, 16.0, false).unwrap();
        assert_eq!((fading.verb.as_str(), fading.verb_alpha), ("Pondering", 0.0));
        assert_eq!(fading.compact(), "Pondering… 19s · 2.1k");
    }

    #[test]
    fn under_reduced_motion_only_the_numbers_change() {
        let mut working = Working::default();
        working.shown("s1", Some(&reading("Fermenting", 14, 1100)), None, 0, 10.0, true);
        let shown = working.shown("s1", Some(&reading("Pondering", 16, 2100)), None, 0, 11.0, true).unwrap();
        assert_eq!((shown.verb_alpha, shown.tokens, shown.elapsed), (1.0, Some(2100), Some(16)));
    }

    #[test]
    fn without_a_status_line_the_turns_start_counts() {
        let mut working = Working::default();
        let shown = working.shown("s1", None, Some(1_000), 1_012, 0.0, false).unwrap();
        assert_eq!((shown.verb.as_str(), shown.elapsed, shown.from_agent), ("Working", Some(12), false));
        assert_eq!(shown.compact(), "Working… 12s");
        let skewed = working.shown("s1", None, Some(1_000), 1_000 + 90_000, 0.0, false).unwrap();
        assert_eq!(skewed.elapsed, None, "clocks a day apart say nothing");
        assert_eq!(working.shown("s1", None, Some(2_000), 1_000, 0.0, false).unwrap().elapsed, Some(0));
    }

    #[test]
    fn durations_and_counts_read_as_the_agents_do() {
        assert_eq!(duration(14), "14s");
        assert_eq!(duration(83), "1m 23s");
        assert_eq!(duration(4912), "1h 21m");
        assert_eq!(tokens(840), "840");
        assert_eq!(tokens(1100), "1.1k");
        assert_eq!(tokens(12_400), "12k");
        assert_eq!(tokens(1_250_000), "1.2M");
    }
}
