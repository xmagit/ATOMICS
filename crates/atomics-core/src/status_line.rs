//! What an agent's own status line says while it works, read off the bottom of its screen: Claude Code's
//! `✢ Fermenting… (14s · ↓ 1.1k tokens · thought for 1s)` and Codex's `• Working (1h 21m 52s • esc to interrupt)`.
//!
//! The line is the agent's, drawn below its transcript; a line of the transcript can look like one. So a line counts
//! only near the bottom, only in one of the two shapes, and only when everything in its parentheses is something a
//! status line says: a time, a token count, how long it thought, or a key to press. Anything else, and it is not one.

use serde::{Deserialize, Serialize};

/// What the status line says, as numbers a client can tick on from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LiveLine {
    /// The words before its ellipsis or parentheses: "Fermenting", "Working", "Running the tests". At most
    /// [`MAX_VERB`] characters.
    pub verb: String,
    /// How long the turn has run, in seconds, as of the reading.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed: Option<u64>,
    /// The tokens it counts, as shown: "1.1k" is 1100.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    /// How long it has thought, in seconds; zero while it thinks without saying for how long.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought: Option<u64>,
}

/// The longest verb kept, in characters.
pub const MAX_VERB: usize = 48;
/// How many lines from the bottom of the screen the status line can be: Claude Code's sits above its prompt box and
/// footer, Codex's above its prompt.
pub const SEARCH_LINES: usize = 12;
/// The longest line read, in characters: a status line is short.
const MAX_LINE: usize = 200;

/// The status line on `screen`, the lowest within [`SEARCH_LINES`] of its bottom; `None` when none shows.
pub fn parse(screen: &str) -> Option<LiveLine> {
    let lines: Vec<&str> = screen.lines().collect();
    let start = lines.len().saturating_sub(SEARCH_LINES);
    (start..lines.len()).rev().find_map(|at| {
        let line = lines[at];
        // A status too long for the screen wraps onto the next line.
        if line.rfind('(') > line.rfind(')')
            && let Some(next) = lines.get(at + 1)
        {
            return line_status(&format!("{} {}", line.trim_end(), next.trim()));
        }
        line_status(line)
    })
}

/// One line as a status line, if it is one.
pub fn line_status(line: &str) -> Option<LiveLine> {
    let line = line.trim();
    if line.is_empty() || line.chars().count() > MAX_LINE {
        return None;
    }
    // Its glyph: a spinner frame or Codex's bullet. Claude Code's own bullets for messages and tool output are not.
    let (glyph, rest) = match line.chars().next()? {
        c if c.is_alphanumeric() => (None, line),
        c => (Some(c), line[c.len_utf8()..].trim_start()),
    };
    if glyph.is_some_and(|g| matches!(g, '⏺' | '⎿' | '>' | '›' | '❯' | '│' | '-' | '#' | '"' | '\'' | '`' | '(' | '['))
    {
        return None;
    }
    if !rest.starts_with(|c: char| c.is_uppercase()) {
        return None;
    }
    let (verb, after, ellipsis) = if let Some(at) = rest.find('…') {
        (&rest[..at], &rest[at + '…'.len_utf8()..], true)
    } else if let Some(at) = rest.find("...") {
        (&rest[..at], &rest[at + 3..], true)
    } else {
        let at = rest.find(" (")?;
        (&rest[..at], &rest[at..], false)
    };
    let verb = verb.trim();
    if verb.is_empty() || verb.chars().count() > MAX_VERB || verb.contains(['(', ')', ':', '`', '"']) {
        return None;
    }
    let after = after.trim();
    let mut live = LiveLine { verb: verb.to_string(), ..LiveLine::default() };
    if after.is_empty() {
        // Claude Code's line before its first second: the spinner and the verb alone.
        return (ellipsis && glyph.is_some()).then_some(live);
    }
    let inside = after.strip_prefix('(')?.strip_suffix(')')?;
    let mut interrupt = false;
    for part in inside.split(['·', '•']).map(str::trim).filter(|part| !part.is_empty()) {
        if let Some(seconds) = duration(part) {
            live.elapsed = Some(seconds);
        } else if let Some(tokens) = tokens(part) {
            live.tokens = Some(tokens);
        } else if let Some(time) = part.strip_prefix("thought for ") {
            live.thought = Some(duration(time)?);
        } else if part == "thinking" || part == "thinking…" {
            live.thought = Some(live.thought.unwrap_or(0));
        } else if key_hint(part) {
            interrupt |= part.to_ascii_lowercase().starts_with("esc to interrupt");
        } else {
            return None;
        }
    }
    // Codex's shape has no ellipsis: its parentheses name the key that interrupts it.
    (ellipsis || interrupt).then_some(live)
}

/// `14s`, `5m`, `1m 23s`, `1h 21m 52s` in seconds.
fn duration(text: &str) -> Option<u64> {
    let mut total = 0u64;
    let mut any = false;
    for part in text.split_whitespace() {
        let unit = part.chars().last()?;
        let number: u64 = part[..part.len() - unit.len_utf8()].parse().ok()?;
        let seconds = match unit {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            _ => return None,
        };
        total = total.checked_add(number.checked_mul(seconds)?)?;
        any = true;
    }
    any.then_some(total).filter(|&total| total < 30 * 86_400)
}

/// `↓ 1.1k tokens`, `↑ 340 tokens`, `12k tokens`.
fn tokens(text: &str) -> Option<u64> {
    let text = text.trim_start_matches(['↓', '↑']).trim();
    let number = text.strip_suffix(" tokens").or_else(|| text.strip_suffix(" token"))?.trim();
    let (digits, scale) = match number.chars().last()? {
        'k' => (&number[..number.len() - 1], 1_000.0),
        'm' | 'M' => (&number[..number.len() - 1], 1_000_000.0),
        _ => (number, 1.0),
    };
    let value: f64 = digits.replace(',', "").parse().ok()?;
    (value.is_finite() && value >= 0.0).then(|| (value * scale).round() as u64)
}

/// A key to press, as status lines name them: `esc to interrupt`, `ctrl+t to hide todos`.
fn key_hint(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let Some((key, _)) = lower.split_once(" to ") else { return false };
    !key.is_empty()
        && key.len() <= 24
        && key.split(['+', ' ']).all(|part| {
            matches!(part, "esc" | "ctrl" | "shift" | "alt" | "tab" | "enter" | "cmd")
                || part.len() == 1 && part.chars().all(|c| c.is_ascii_lowercase())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(verb: &str, elapsed: Option<u64>, tokens: Option<u64>, thought: Option<u64>) -> Option<LiveLine> {
        Some(LiveLine { verb: verb.into(), elapsed, tokens, thought })
    }

    #[test]
    fn claude_codes_line_with_and_without_tokens_and_thinking() {
        assert_eq!(
            line_status("✢ Fermenting… (14s · ↓ 1.1k tokens · thought for 1s)"),
            line("Fermenting", Some(14), Some(1100), Some(1))
        );
        assert_eq!(line_status("✻ Building… (5m · esc to interrupt)"), line("Building", Some(300), None, None));
        assert_eq!(
            line_status("✽ Cogitating… (1m 23s · ↑ 340 tokens · esc to interrupt)"),
            line("Cogitating", Some(83), Some(340), None)
        );
        assert_eq!(line_status("· Thinking… (esc to interrupt)"), line("Thinking", None, None, None));
        assert_eq!(line_status("✳ Pondering…"), line("Pondering", None, None, None), "before its first second");
        assert_eq!(
            line_status(
                "✶ Running the tests… (2m 3s · ↓ 5.2k tokens · thinking · esc to interrupt · ctrl+t to hide todos)"
            ),
            line("Running the tests", Some(123), Some(5200), Some(0)),
            "a todo's words, and its key"
        );
    }

    #[test]
    fn codexs_line_with_hours() {
        assert_eq!(line_status("Working (1h 21m 52s • esc to interrupt)"), line("Working", Some(4912), None, None));
        assert_eq!(line_status("• Working (5s • esc to interrupt)"), line("Working", Some(5), None, None));
        assert_eq!(
            line_status("• Investigating the parser (32s • esc to interrupt)"),
            line("Investigating the parser", Some(32), None, None)
        );
    }

    #[test]
    fn status_duration_rejects_overflow_and_the_thirty_day_boundary() {
        for time in ["18446744073709551615h", "1s 18446744073709551615s", "30d", "29d 24h"] {
            assert_eq!(line_status(&format!("✢ Working… ({time})")), None);
            assert_eq!(line_status(&format!("✢ Working… (3s · thought for {time})")), None);
        }
        assert_eq!(duration("29d 23h 59m 59s"), Some(30 * 86_400 - 1));
    }

    #[test]
    fn what_only_looks_like_one_is_not() {
        for text in [
            // A finished turn.
            "✻ Worked for 5m 3s",
            "─ Worked for 1m 05s ─",
            // Claude Code's own bullets: a message, a tool call and its output.
            "⏺ Reading the parser… (from the README)",
            "⏺ Bash(cargo test)",
            "  ⎿  Running… (14s · esc to interrupt)",
            // Transcript prose and quotes.
            "The build says Working (5s • esc to interrupt) when busy.",
            "Working (see the notes below)",
            "> Working (5s • esc to interrupt)",
            "› Fermenting… (14s)",
            "✢ Fermenting… (14s · see docs/status.md)",
            "✢ Fermenting… (14s",
            "✢ fermenting… (14s)",
            "Note: Fermenting… (14s)",
            "Working (5s)",
            "✢ Fermenting… (99999999999999999999s)",
            "",
        ] {
            assert_eq!(line_status(text), None, "{text:?}");
        }
    }

    #[test]
    fn the_lowest_line_near_the_bottom_counts_and_a_wrapped_one_joins() {
        let screen = "⏺ Earlier: ✢ Fermenting… (1s)\n\
                      ✢ Fermenting… (14s · ↓ 1.1k tokens)\n\
                      ╭────────────╮\n\
                      │ >          │\n\
                      ╰────────────╯\n\
                        ⏵⏵ auto mode on · 1 shell";
        assert_eq!(parse(screen), line("Fermenting", Some(14), Some(1100), None));
        let wrapped = "✢ Fermenting… (14s · ↓ 1.1k tokens · thought\nfor 1s)\n> ";
        assert_eq!(parse(wrapped), line("Fermenting", Some(14), Some(1100), Some(1)));
        // Too far up: the transcript's, not the live line.
        let far = format!("✢ Fermenting… (14s)\n{}", "text\n".repeat(SEARCH_LINES));
        assert_eq!(parse(&far), None);
        let long = format!("✢ {}… (14s)", "Very long verb ".repeat(10));
        assert_eq!(parse(&long), None, "a verb longer than a status line's");
    }
}
