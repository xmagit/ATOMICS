//! A small markdown reader for ticket text: headings, lists, quotes, code blocks and the
//! inline bold, code and links. Images are links. Nothing here fetches or runs anything.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    Text(String),
    Bold(String),
    Italic(String),
    Code(String),
    Link { text: String, url: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Heading(u8, Vec<Inline>),
    Paragraph(Vec<Inline>),
    Table {
        header: Vec<Vec<Inline>>,
        align: Vec<egui::Align>,
        rows: Vec<Vec<Vec<Inline>>>,
    },
    /// A list item: nesting depth, its marker (`•` or `1.`) and its text.
    Item(usize, String, Vec<Inline>),
    Quote(Vec<Inline>),
    Code {
        language: String,
        text: String,
    },
    Rule,
}

pub fn parse(source: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut paragraph: Vec<&str> = Vec::new();
    let mut code: Option<Vec<&str>> = None;
    let mut language = "";
    fn flush(blocks: &mut Vec<Block>, paragraph: &mut Vec<&str>) {
        if !paragraph.is_empty() {
            blocks.push(Block::Paragraph(inlines(&paragraph.join(" "))));
            paragraph.clear();
        }
    }
    let mut lines = source.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        if let Some(lines) = code.as_mut() {
            if trimmed.starts_with("```") {
                blocks.push(Block::Code { language: language.into(), text: lines.join("\n") });
                code = None;
            } else {
                lines.push(line);
            }
            continue;
        }
        if let Some(info) = trimmed.strip_prefix("```") {
            language = info.split_whitespace().next().unwrap_or_default();
            flush(&mut blocks, &mut paragraph);
            code = Some(Vec::new());
        } else if trimmed.is_empty() {
            flush(&mut blocks, &mut paragraph);
        } else if trimmed.contains('|')
            && let Some(align) = lines.peek().and_then(|next| table_alignment(next))
            && let header = table_cells(trimmed)
            && header.len() == align.len()
        {
            flush(&mut blocks, &mut paragraph);
            lines.next();
            let mut rows = Vec::new();
            while let Some(next) = lines.next_if(|line| {
                let line = line.trim_start();
                line.contains('|')
                    && !line.starts_with("```")
                    && !line.starts_with('>')
                    && heading(line).is_none()
                    && item(line).is_none()
            }) {
                let mut cells = table_cells(next);
                cells.resize(header.len(), String::new());
                rows.push(cells.iter().map(|cell| inlines(cell)).collect());
            }
            blocks.push(Block::Table { header: header.iter().map(|cell| inlines(cell)).collect(), align, rows });
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
        blocks.push(Block::Code { language: language.into(), text: lines.join("\n") });
    }
    flush(&mut blocks, &mut paragraph);
    blocks
}

/// Split a pipe row without treating an escaped pipe as a column boundary.
fn table_cells(line: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut chars = line.trim().strip_prefix('|').unwrap_or(line.trim()).chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' if chars.peek() == Some(&'|') => {
                chars.next();
                cell.push('|');
            }
            '|' => {
                cells.push(cell.trim().to_owned());
                cell.clear();
            }
            _ => cell.push(ch),
        }
    }
    if !cell.trim().is_empty() || !line.trim_end().ends_with('|') {
        cells.push(cell.trim().to_owned());
    }
    cells
}

fn table_alignment(line: &str) -> Option<Vec<egui::Align>> {
    if !line.contains('|') {
        return None;
    }
    let cells = table_cells(line);
    if cells.is_empty() {
        return None;
    }
    cells
        .iter()
        .map(|cell| {
            let dashes = cell.strip_prefix(':').unwrap_or(cell);
            let dashes = dashes.strip_suffix(':').unwrap_or(dashes);
            if dashes.is_empty() || !dashes.bytes().all(|b| b == b'-') {
                return None;
            }
            Some(match (cell.starts_with(':'), cell.ends_with(':')) {
                (true, true) => egui::Align::Center,
                (false, true) => egui::Align::Max,
                _ => egui::Align::Min,
            })
        })
        .collect()
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
            '*' | '_' => {
                let doubled = if c == '*' { "**" } else { "__" };
                let strong = rest.starts_with(doubled);
                let delimiter = if strong {
                    doubled
                } else if c == '*' {
                    "*"
                } else {
                    "_"
                };
                let inner = &rest[delimiter.len()..];
                let at_word_start = c != '_' || !text[..text.len() - rest.len()].ends_with(char::is_alphanumeric);
                at_word_start
                    .then(|| inner.find(delimiter))
                    .flatten()
                    .filter(|&end| {
                        end > 0
                            && !inner.starts_with(char::is_whitespace)
                            && !inner[..end].ends_with(char::is_whitespace)
                            && (c != '_' || !inner[end + delimiter.len()..].starts_with(char::is_alphanumeric))
                    })
                    .map(|end| {
                        let body = inner[..end].to_owned();
                        (if strong { Inline::Bold(body) } else { Inline::Italic(body) }, end + 2 * delimiter.len())
                    })
            }
            '`' => rest[1..].find('`').filter(|n| *n > 0).map(|n| (Inline::Code(rest[1..1 + n].to_string()), n + 2)),
            '[' | '!' => link(rest),
            _ => None,
        };
        match found {
            Some((inline, used)) => {
                push(&mut out, &mut plain);
                match inline {
                    Inline::Bold(body) => emphasized(&mut out, &body, Inline::Bold),
                    Inline::Italic(body) => emphasized(&mut out, &body, Inline::Italic),
                    other => out.push(other),
                }
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

fn emphasized(out: &mut Vec<Inline>, mut body: &str, style: fn(String) -> Inline) {
    while let Some((before, rest)) = body.split_once('`')
        && let Some((code, after)) = rest.split_once('`')
    {
        if !before.is_empty() {
            out.push(style(before.to_owned()));
        }
        out.push(Inline::Code(code.to_owned()));
        body = after;
    }
    if !body.is_empty() {
        out.push(style(body.to_owned()));
    }
}

/// The words of a conversation title, without its inline formatting.
pub fn plain_title(source: &str) -> String {
    let line = source.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or_default();
    inlines(heading(line).map_or(line, |(_, title)| title))
        .into_iter()
        .map(|span| match span {
            Inline::Text(s) | Inline::Bold(s) | Inline::Italic(s) | Inline::Code(s) | Inline::Link { text: s, .. } => s,
        })
        .collect()
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

/// The shared check for every URL opened in a browser, including URLs supplied by a host or agent.
/// Keep the authority explicit and refuse characters browsers might silently normalize.
/// International hosts must be spelled as ASCII punycode so hints cannot show look-alikes.
pub fn web_url(url: &str) -> Option<&str> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    ((scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        && !authority.is_empty()
        && authority.is_ascii()
        && !authority.contains('%')
        && !url.chars().any(|c| c.is_whitespace() || hidden_url_char(c) || c == '\\'))
    .then_some(url)
}

fn hidden_url_char(c: char) -> bool {
    use unicode_general_category::{GeneralCategory, get_general_category};
    matches!(get_general_category(c), GeneralCategory::Control | GeneralCategory::Format)
        // Other invisible characters, including fillers and variation selectors.
        || matches!(c,
            '\u{34f}' | '\u{115f}'..='\u{1160}' | '\u{17b4}'..='\u{17b5}' | '\u{180b}'..='\u{180f}'
            | '\u{2060}'..='\u{206f}' | '\u{3164}' | '\u{fe00}'..='\u{fe0f}' | '\u{ffa0}'
            | '\u{e0000}'..='\u{e0fff}')
}

/// Show the parsed destination before the full URL, whose user-info can resemble another host.
pub fn web_url_hover(url: &str) -> String {
    let host = url::Url::parse(url).ok().and_then(|address| address.host_str().map(str::to_owned));
    match host {
        Some(host) => format!("Open {host}\n{url}"),
        None => format!("Unparseable URL\n{url}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_markdown_preserves_code_inside_emphasis() {
        let spans = inlines("Open **`recipes.py`** and *use `search` now*.");
        assert!(spans.contains(&Inline::Code("recipes.py".into())));
        assert!(spans.contains(&Inline::Code("search".into())));
        assert!(spans.contains(&Inline::Italic("use ".into())));
    }

    #[test]
    fn link_hover_identifies_the_host_after_user_info() {
        assert_eq!(
            web_url_hover("https://github.com@evil.example/path"),
            "Open evil.example\nhttps://github.com@evil.example/path"
        );
        assert_eq!(web_url_hover("https://xn--gthub-n2e.com"), "Open xn--gthub-n2e.com\nhttps://xn--gthub-n2e.com");
        assert_eq!(web_url_hover("invalid"), "Unparseable URL\ninvalid");
    }

    #[test]
    fn a_table_keeps_inline_styles_escaped_pipes_alignment_and_empty_cells() {
        let blocks = parse(
            "a | b is prose\n\nPage | Notes | Agent\n:--- | :---: | ---:\n`sun.html` | light \\| dark | **Haiku**\nmoon | phases\n\nAfter",
        );
        assert_eq!(blocks[0], Block::Paragraph(vec![Inline::Text("a | b is prose".into())]));
        let Block::Table { header, align, rows } = &blocks[1] else { panic!("missing table") };
        assert_eq!(header.len(), 3);
        assert_eq!(align, &[egui::Align::Min, egui::Align::Center, egui::Align::Max]);
        assert_eq!(
            rows[0],
            [
                vec![Inline::Code("sun.html".into())],
                vec![Inline::Text("light | dark".into())],
                vec![Inline::Bold("Haiku".into())]
            ]
        );
        assert!(rows[1][2].is_empty());
        assert_eq!(blocks[2], Block::Paragraph(vec![Inline::Text("After".into())]));
    }

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
        assert_eq!(blocks[6], Block::Code { language: "rs".into(), text: "let a = 1;\n# not a heading".into() });
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
    fn browser_urls_allow_only_explicit_http_and_https_addresses() {
        for url in ["http://example.com", "https://example.com/path?q=1#part", "HTTPS://example.com"] {
            assert_eq!(web_url(url), Some(url));
        }
        for url in [
            "javascript:alert(1)",
            "data:text/html,test",
            "file:///tmp/test",
            "mailto:mail@example.com",
            "httpx://example.com",
            "https:example.com",
            "//example.com",
            "https:///missing",
            "https://?query",
            "https://#fragment",
            "https://",
            " https://example.com",
            "https://example.com/\nfile:///tmp/test",
            "https://example.com/\u{7f}",
            "https://example.com\\file",
        ] {
            assert_eq!(web_url(url), None, "{url:?}");
        }
    }

    #[test]
    fn browser_urls_refuse_hidden_characters_and_unicode_look_alike_hosts() {
        for hidden in ['\u{202e}', '\u{200b}', '\u{200d}', '\u{feff}', '\u{ad}', '\u{34f}', '\u{fff9}', '\u{fe0f}'] {
            for url in [
                format!("https://example.com/{hidden}path"),
                format!("https://example.com/?q={hidden}"),
                format!("https://example.com/#{hidden}"),
            ] {
                assert_eq!(web_url(&url), None, "{url:?}");
            }
        }
        for url in ["https://g\u{456}thub.com/repo", "https://\u{ff47}ithub.com/repo", "https://g%D1%96thub.com/repo"] {
            assert_eq!(web_url(url), None, "{url:?}");
        }
        for url in ["https://xn--gthub-n2e.com/repo", "https://example.com/文档", "https://example.com/a%20b"] {
            assert_eq!(web_url(url), Some(url));
        }
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
