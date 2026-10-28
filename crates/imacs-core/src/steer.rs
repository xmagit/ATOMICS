//! Turning review comments and typed messages into input for an agent.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewComment {
    pub path: String,
    /// Line in the new file, or in the old file for removed lines.
    pub line: Option<u32>,
    pub removed: bool,
    pub excerpt: String,
    pub body: String,
}

/// One message with every comment, in the order they were written.
pub fn compose_review(header: &str, comments: &[ReviewComment]) -> String {
    let mut out = String::from(header);
    out.push('\n');
    for (i, c) in comments.iter().enumerate() {
        let location = match (c.line, c.removed) {
            (Some(line), false) => format!("{}:{line}", c.path),
            (Some(line), true) => format!("{} (removed line {line})", c.path),
            (None, _) => c.path.clone(),
        };
        out.push_str(&format!("\n{}. {location}\n", i + 1));
        let excerpt = c.excerpt.trim();
        if !excerpt.is_empty() {
            out.push_str(&format!("   > {}\n", truncate(excerpt, 160)));
        }
        for line in c.body.trim().lines() {
            out.push_str(&format!("   {line}\n"));
        }
    }
    out.trim_end().to_string()
}

/// Bytes that type `text` into a terminal program as one message: a bracketed
/// paste when the program asked for it, so newlines don't submit early.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let text = text.replace("\r\n", "\n");
    if bracketed {
        let clean = text.replace("\x1b[201~", "");
        format!("\x1b[200~{clean}\x1b[201~").into_bytes()
    } else {
        text.replace('\n', "\r").into_bytes()
    }
}

/// A complete message above the terminal's last prompt, including Codex's visible queue.
pub fn message_in_transcript(screen: &str, text: &str) -> bool {
    let lines: Vec<_> = screen.lines().collect();
    let end = lines.iter().rposition(|line| line.trim_start().starts_with(['›', '❯']));
    let Some(end) = end else { return false };
    let transcript: String = lines[..end].iter().flat_map(|line| line.chars().filter(|c| !c.is_whitespace())).collect();
    let message: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    !message.is_empty() && transcript.contains(&message)
}

fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_lists_locations_excerpts_and_bodies() {
        let comments = vec![
            ReviewComment {
                path: "src/Foo.cs".into(),
                line: Some(120),
                removed: false,
                excerpt: "if (x == null)".into(),
                body: "Use `is null`.".into(),
            },
            ReviewComment {
                path: "src/Bar.cs".into(),
                line: Some(44),
                removed: true,
                excerpt: String::new(),
                body: "Why was this removed?\nIt guarded the retry.".into(),
            },
        ];
        let text = compose_review("Review:", &comments);
        assert_eq!(
            text,
            "Review:\n\n1. src/Foo.cs:120\n   > if (x == null)\n   Use `is null`.\n\n2. src/Bar.cs (removed line 44)\n   Why was this removed?\n   It guarded the retry."
        );
    }

    #[test]
    fn paste_is_bracketed_and_cannot_be_escaped() {
        assert_eq!(paste_bytes("a\nb", true), b"\x1b[200~a\nb\x1b[201~");
        assert_eq!(paste_bytes("x\x1b[201~y", true), b"\x1b[200~xy\x1b[201~");
        assert_eq!(paste_bytes("a\nb", false), b"a\rb");
    }

    #[test]
    fn delivery_needs_the_complete_message_outside_the_composer() {
        let text = "Please check the first change and then the second change";
        assert!(!message_in_transcript(&format!("Working\n› {text}\n\n tab to queue"), text));
        assert!(!message_in_transcript("Please check the first change\n› ", text));
        assert!(!message_in_transcript("Working\n› \n", "\n "));
        assert!(message_in_transcript("• Please check the first change and then\n  the second change\n› ", text));
        assert!(message_in_transcript("› Please check the first change and then the second change\nWorking\n› ", text));
    }
}
