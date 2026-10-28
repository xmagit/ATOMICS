//! Syntax highlighting for diff lines with syntect and bat's grammar set. syntect
//! works line by line, which suits hunks that start mid-file.

use egui::Color32;
use imacs_core::diff::{FileDiff, LineKind};
use syntect::easy::HighlightLines;
use syntect::highlighting::Theme;
use syntect::parsing::SyntaxSet;
use two_face::theme::EmbeddedThemeName;

/// A colored byte range of a line. Small, since every line of a diff keeps a few.
#[derive(Clone, Copy)]
pub struct Span {
    pub start: u32,
    pub end: u32,
    pub color: Color32,
}

pub type Spans = Vec<Span>;

pub struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

impl Highlighter {
    pub fn new(theme_name: &str) -> Highlighter {
        let themes = two_face::theme::extra();
        let wanted = theme_name.to_ascii_lowercase().replace([' ', '-', '_'], "");
        let name = two_face::theme::EmbeddedLazyThemeSet::theme_names()
            .iter()
            .copied()
            .find(|n| n.as_name().to_ascii_lowercase().replace([' ', '-', '_'], "") == wanted)
            .unwrap_or(EmbeddedThemeName::OneHalfDark);
        Highlighter { syntaxes: two_face::syntax::extra_newlines(), theme: themes.get(name).clone() }
    }

    /// Colors per line, hunk by hunk. New-side lines and removed lines are
    /// highlighted with separate parser states; context lines feed both.
    pub fn file(&self, file: &FileDiff) -> Vec<Vec<Spans>> {
        if file.binary {
            return Vec::new();
        }
        let syntax = self.syntax_for(file.path());

        file.hunks
            .iter()
            .map(|hunk| {
                let mut new_side = HighlightLines::new(syntax, &self.theme);
                let mut old_side = HighlightLines::new(syntax, &self.theme);
                hunk.lines
                    .iter()
                    .map(|line| {
                        let text = format!("{}\n", line.text);
                        match line.kind {
                            LineKind::Removed => self.spans(&mut old_side, &text),
                            LineKind::Added => self.spans(&mut new_side, &text),
                            LineKind::Context => {
                                let _ = self.spans(&mut old_side, &text);
                                self.spans(&mut new_side, &text)
                            }
                            LineKind::Note => Vec::new(),
                        }
                    })
                    .collect()
            })
            .collect()
    }

    /// Colors for every line of a whole file.
    pub fn lines(&self, path: &str, lines: &[String]) -> Vec<Spans> {
        let syntax = self.syntax_for(path);
        let mut state = HighlightLines::new(syntax, &self.theme);
        lines.iter().map(|line| self.spans(&mut state, &format!("{line}\n"))).collect()
    }

    fn syntax_for(&self, path: &str) -> &syntect::parsing::SyntaxReference {
        let extension = path.rsplit('.').next().unwrap_or("");
        let file_name = path.rsplit('/').next().unwrap_or(path);
        self.syntaxes
            .find_syntax_by_extension(extension)
            .or_else(|| self.syntaxes.find_syntax_by_extension(file_name))
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text())
    }

    fn spans(&self, state: &mut HighlightLines, text: &str) -> Spans {
        let Ok(regions) = state.highlight_line(text, &self.syntaxes) else { return Vec::new() };
        let mut pos = 0;
        let mut spans = Vec::with_capacity(regions.len());
        for (style, piece) in regions {
            let end = (pos + piece.len()).min(text.len() - 1);
            if end > pos {
                let c = style.foreground;
                spans.push(Span { start: pos as u32, end: end as u32, color: Color32::from_rgb(c.r, c.g, c.b) });
            }
            pos += piece.len();
        }
        spans
    }
}
