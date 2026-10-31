//! A small markdown reader for ticket text: headings, lists, quotes, code blocks and the
//! inline bold, code and links. Images are links. Nothing here fetches or runs anything.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    Text(String),
    Bold(String),
    Code(String),
    Link { text: String, url: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Heading(u8, Vec<Inline>),
    Paragraph(Vec<Inline>),
    /// A list item: nesting depth, its marker (`•` or `1.`) and its text.
    Item(usize, String, Vec<Inline>),
    Quote(Vec<Inline>),
    Code(String),
    Rule,
}

pub fn parse(source: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut paragraph: Vec<&str> = Vec::new();
    let mut code: Option<Vec<&str>> = None;
    fn flush(blocks: &mut Vec<Block>, paragraph: &mut Vec<&str>) {
        if !paragraph.is_empty() {
            blocks.push(Block::Paragraph(inlines(&paragraph.join(" "))));
            paragraph.clear();
        }
    }
    for line in source.lines() {
        let trimmed = line.trim_start();
        if let Some(lines) = code.as_mut() {
            if trimmed.starts_with("```") {
                blocks.push(Block::Code(lines.join("\n")));
                code = None;
            } else {
                lines.push(line);
            }
            continue;
        }
        if trimmed.starts_with("```") {
            flush(&mut blocks, &mut paragraph);
            code = Some(Vec::new());
        } else if trimmed.is_empty() {
            flush(&mut blocks, &mut paragraph);
        } else if let Some((level, text)) = heading(trimmed) {
            flush(&mut blocks, &mut paragraph);
            blocks.push(Block::Heading(level, inlines(text)));
        } else if matches!(trimmed, "---" | "***" | "___") {
            flush(&mut blocks, &mut paragraph);
            blocks.push(Block::Rule);
        } else if let Some((marker, text)) = item(trimmed) {
            flush(&mut blocks, &mut paragraph);
            let depth = (line.len() - trimmed.len()) / 2;
            blocks.push(Block::Item(depth, marker, inlines(text)));
        } else if let Some(text) = trimmed.strip_prefix('>') {
            flush(&mut blocks, &mut paragraph);
            blocks.push(Block::Quote(inlines(text.trim())));
        } else {
            paragraph.push(trimmed);
        }
    }
    if let Some(lines) = code {
        blocks.push(Block::Code(lines.join("\n")));
    }
    flush(&mut blocks, &mut paragraph);
    blocks
}

/// `# Title` to (1, "Title"). Needs the space, so `#tag` stays text.
pub fn heading(line: &str) -> Option<(u8, &str)> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    let rest = line.get(hashes..)?;
    ((1..=6).contains(&hashes) && rest.starts_with(' ')).then(|| (hashes as u8, rest.trim()))
}

fn item(line: &str) -> Option<(String, &str)> {
    if let Some(text) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")).or_else(|| line.strip_prefix("+ "))
    {
        return Some(("•".into(), text));
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    let rest = line.get(digits..)?;
    (digits > 0 && digits < 4 && (rest.starts_with(". ") || rest.starts_with(") ")))
        .then(|| (format!("{}.", &line[..digits]), rest[2..].trim_start()))
}

pub fn inlines(text: &str) -> Vec<Inline> {
    let mut out = Vec::new();
    let mut plain = String::new();
    let mut rest = text;
    fn push(out: &mut Vec<Inline>, plain: &mut String) {
        if !plain.is_empty() {
            out.push(Inline::Text(std::mem::take(plain)));
        }
    }
    while let Some(c) = rest.chars().next() {
        let found = match c {
            '*' if rest.starts_with("**") => {
                rest[2..].find("**").filter(|n| *n > 0).map(|n| (Inline::Bold(rest[2..2 + n].to_string()), n + 4))
            }
            '`' => rest[1..].find('`').filter(|n| *n > 0).map(|n| (Inline::Code(rest[1..1 + n].to_string()), n + 2)),
            '[' | '!' => link(rest),
            _ => None,
        };
        match found {
            Some((inline, used)) => {
                push(&mut out, &mut plain);
                out.push(inline);
                rest = &rest[used..];
            }
            None => {
                plain.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    push(&mut out, &mut plain);
    out
}

/// `[text](url)` or `![alt](url)` at the start of `rest`, and how many bytes it takes.
fn link(rest: &str) -> Option<(Inline, usize)> {
    let (body, bang) = match rest.strip_prefix('!') {
        Some(body) => (body, 1),
        None => (rest, 0),
    };
    let inner = body.strip_prefix('[')?;
    let close = inner.find("](")?;
    let end = inner[close + 2..].find(')')?;
    let text = &inner[..close];
    let url = inner[close + 2..close + 2 + end].trim();
    if url.is_empty() || url.contains(char::is_whitespace) || text.contains('[') {
        return None;
    }
    let text = if text.is_empty() { url.to_string() } else { text.to_string() };
    Some((Inline::Link { text, url: url.to_string() }, bang + 1 + close + 2 + end + 1))
}

/// Description split at its headings: (heading text, markdown of the part including its
/// heading line). Text before the first heading is a part of its own, without a title.
pub fn sections(source: &str) -> Vec<(String, String)> {
    let mut parts: Vec<(String, String)> = Vec::new();
    let mut fenced = false;
    for line in source.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            fenced = !fenced;
        }
        if !fenced && let Some((_, title)) = heading(trimmed) {
            parts.push((title.to_string(), String::new()));
        } else if parts.is_empty() {
            parts.push((String::new(), String::new()));
        }
        let part = parts.last_mut().expect("a part exists");
        part.1.push_str(line);
        part.1.push('\n');
    }
    parts.retain(|(title, text)| !title.is_empty() || !text.trim().is_empty());
    parts
}

/// The URL to open for a markdown link: web addresses only.
pub fn web_url(url: &str) -> Option<&str> {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://")).then_some(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_cover_headings_lists_quotes_code_and_paragraphs() {
        let blocks = parse(
            "# Goal\nfirst line\nsecond **bold** `x`\n\n- one\n  - two\n1. three\n> quoted\n```rs\nlet a = 1;\n# not a heading\n```\n---\n#tag",
        );
        assert_eq!(blocks[0], Block::Heading(1, vec![Inline::Text("Goal".into())]));
        assert_eq!(
            blocks[1],
            Block::Paragraph(vec![
                Inline::Text("first line second ".into()),
                Inline::Bold("bold".into()),
                Inline::Text(" ".into()),
                Inline::Code("x".into()),
            ])
        );
        assert!(matches!(&blocks[2], Block::Item(0, m, _) if m == "•"));
        assert!(matches!(&blocks[3], Block::Item(1, m, _) if m == "•"));
        assert!(matches!(&blocks[4], Block::Item(0, m, _) if m == "1."));
        assert!(matches!(&blocks[5], Block::Quote(_)));
        assert_eq!(blocks[6], Block::Code("let a = 1;\n# not a heading".into()));
        assert_eq!(blocks[7], Block::Rule);
        assert_eq!(blocks[8], Block::Paragraph(vec![Inline::Text("#tag".into())]));
    }

    #[test]
    fn links_and_images_are_links_and_broken_ones_stay_text() {
        assert_eq!(
            inlines("see [docs](https://a.example/x) and ![shot](https://a.example/i.png)"),
            vec![
                Inline::Text("see ".into()),
                Inline::Link { text: "docs".into(), url: "https://a.example/x".into() },
                Inline::Text(" and ".into()),
                Inline::Link { text: "shot".into(), url: "https://a.example/i.png".into() },
            ]
        );
        assert_eq!(inlines("[a](b c) **open"), vec![Inline::Text("[a](b c) **open".into())]);
        assert_eq!(web_url("https://x.example"), Some("https://x.example"));
        assert_eq!(web_url("javascript:alert(1)"), None);
        assert_eq!(web_url("file:///etc/passwd"), None);
    }

    #[test]
    fn sections_split_at_headings_outside_code() {
        let parts = sections("intro\n## Goal\ntext\n```\n## no\n```\n## Order\n1. a");
        let titles: Vec<_> = parts.iter().map(|p| p.0.as_str()).collect();
        assert_eq!(titles, ["", "Goal", "Order"]);
        assert!(parts[1].1.contains("## no"));
        assert!(sections("  \n").is_empty());
    }
}
