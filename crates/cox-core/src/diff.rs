//! Unified diff model and parser for `git diff` output, plus word-level emphasis
//! for lines that were edited rather than replaced.

use std::ops::Range;

use similar::{Algorithm, DiffOp, capture_diff_slices};

#[derive(Debug, Clone, Default)]
pub struct WorktreeDiff {
    pub base_ref: String,
    /// Merge-base commit the diff is taken against.
    pub base: String,
    pub files: Vec<FileDiff>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
    Untracked,
}

#[derive(Debug, Clone)]
pub struct FileDiff {
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub status: FileStatus,
    pub binary: bool,
    pub hunks: Vec<Hunk>,
}

#[derive(Debug, Clone)]
pub struct Hunk {
    pub header: String,
    pub old_start: u32,
    pub new_start: u32,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
    /// `\ No newline at end of file`
    Note,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: String,
    /// Byte ranges of `text` that changed within an edited line.
    pub emphasis: Vec<Range<usize>>,
}

impl WorktreeDiff {
    pub fn additions(&self) -> usize {
        self.files.iter().map(FileDiff::additions).sum()
    }

    pub fn deletions(&self) -> usize {
        self.files.iter().map(FileDiff::deletions).sum()
    }
}

impl FileDiff {
    /// The path to show: the new path, or the old one for deletions.
    pub fn path(&self) -> &str {
        self.new_path.as_deref().or(self.old_path.as_deref()).unwrap_or("?")
    }

    pub fn additions(&self) -> usize {
        self.count(LineKind::Added)
    }

    pub fn deletions(&self) -> usize {
        self.count(LineKind::Removed)
    }

    fn count(&self, kind: LineKind) -> usize {
        self.hunks.iter().flat_map(|h| &h.lines).filter(|l| l.kind == kind).count()
    }

    /// A whole new file shown as one all-added hunk (for untracked files).
    pub fn untracked(path: String, content: &str) -> FileDiff {
        let lines: Vec<DiffLine> = content
            .lines()
            .enumerate()
            .map(|(i, line)| DiffLine {
                kind: LineKind::Added,
                old_no: None,
                new_no: Some(i as u32 + 1),
                text: line.to_string(),
                emphasis: Vec::new(),
            })
            .collect();
        let header = format!("@@ -0,0 +1,{} @@", lines.len());
        FileDiff {
            old_path: None,
            new_path: Some(path),
            status: FileStatus::Untracked,
            binary: false,
            hunks: if lines.is_empty() { Vec::new() } else { vec![Hunk { header, old_start: 0, new_start: 1, lines }] },
        }
    }
}

/// Parses `git diff` output (no color, `a/` and `b/` prefixes).
pub fn parse_unified(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut old_no = 0;
    let mut new_no = 0;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let (old, new) = split_git_header(rest);
            files.push(FileDiff {
                old_path: old,
                new_path: new,
                status: FileStatus::Modified,
                binary: false,
                hunks: Vec::new(),
            });
            continue;
        }
        let Some(file) = files.last_mut() else { continue };

        if let Some(hunk) = file.hunks.last_mut()
            && let Some(first) = line.chars().next()
            && matches!(first, ' ' | '+' | '-' | '\\')
        {
            let text = line[1..].to_string();
            let (kind, old, new) = match first {
                '+' => {
                    new_no += 1;
                    (LineKind::Added, None, Some(new_no))
                }
                '-' => {
                    old_no += 1;
                    (LineKind::Removed, Some(old_no), None)
                }
                ' ' => {
                    old_no += 1;
                    new_no += 1;
                    (LineKind::Context, Some(old_no), Some(new_no))
                }
                _ => (LineKind::Note, None, None),
            };
            hunk.lines.push(DiffLine { kind, old_no: old, new_no: new, text, emphasis: Vec::new() });
            continue;
        }

        if line.starts_with("@@") {
            let (old_start, new_start) = parse_hunk_header(line);
            old_no = old_start.saturating_sub(1);
            new_no = new_start.saturating_sub(1);
            file.hunks.push(Hunk { header: line.to_string(), old_start, new_start, lines: Vec::new() });
        } else if line.starts_with("new file mode") {
            file.status = FileStatus::Added;
        } else if line.starts_with("deleted file mode") {
            file.status = FileStatus::Deleted;
        } else if let Some(path) = line.strip_prefix("rename from ") {
            file.status = FileStatus::Renamed;
            file.old_path = Some(unquote(path));
        } else if let Some(path) = line.strip_prefix("rename to ") {
            file.status = FileStatus::Renamed;
            file.new_path = Some(unquote(path));
        } else if let Some(path) = line.strip_prefix("--- ") {
            file.old_path = strip_side(path, "a/");
        } else if let Some(path) = line.strip_prefix("+++ ") {
            file.new_path = strip_side(path, "b/");
        } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
            file.binary = true;
        }
    }

    for file in &mut files {
        match file.status {
            FileStatus::Added => file.old_path = None,
            FileStatus::Deleted => file.new_path = None,
            _ => {}
        }
        for hunk in &mut file.hunks {
            emphasize(&mut hunk.lines);
        }
    }
    files
}

fn parse_hunk_header(line: &str) -> (u32, u32) {
    // @@ -12,7 +12,9 @@ optional section
    let mut parts = line.split_whitespace().skip(1);
    let start =
        |part: Option<&str>| part.and_then(|p| p[1..].split(',').next()).and_then(|n| n.parse().ok()).unwrap_or(0);
    let old = start(parts.next());
    let new = start(parts.next());
    (old, new)
}

fn strip_side(path: &str, prefix: &str) -> Option<String> {
    let path = unquote(path.trim_end_matches('\t'));
    if path == "/dev/null" {
        return None;
    }
    Some(path.strip_prefix(prefix).map(str::to_string).unwrap_or(path))
}

/// `a/x b/y` — exact when both sides are equal, which covers everything but renames
/// (and renames carry `rename from`/`rename to` lines).
fn split_git_header(rest: &str) -> (Option<String>, Option<String>) {
    if rest.starts_with('"') {
        let mut parts = rest.splitn(2, "\" ");
        let old = parts.next().map(|p| unquote(&format!("{p}\"")));
        let new = parts.next().map(unquote);
        let strip =
            |p: Option<String>, prefix: &str| p.map(|p| p.strip_prefix(prefix).map(str::to_string).unwrap_or(p));
        return (strip(old, "a/"), strip(new, "b/"));
    }
    let body = rest.strip_prefix("a/").unwrap_or(rest);
    let half = body.len().saturating_sub(3) / 2;
    if body.len() >= 3
        && body.is_char_boundary(half)
        && body[half..].starts_with(" b/")
        && body[..half] == body[half + 3..]
    {
        let path = body[..half].to_string();
        return (Some(path.clone()), Some(path));
    }
    match body.split_once(" b/") {
        Some((old, new)) => (Some(old.to_string()), Some(new.to_string())),
        None => (Some(body.to_string()), Some(body.to_string())),
    }
}

/// Undoes git's C-style quoting of unusual paths.
fn unquote(path: &str) -> String {
    let Some(inner) = path.strip_prefix('"').and_then(|p| p.strip_suffix('"')) else {
        return path.to_string();
    };
    let mut bytes = Vec::with_capacity(inner.len());
    let mut chars = inner.bytes().peekable();
    while let Some(b) = chars.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        match chars.next() {
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(d @ b'0'..=b'7') => {
                let mut value = (d - b'0') as u32;
                for _ in 0..2 {
                    if let Some(&next @ b'0'..=b'7') = chars.peek() {
                        value = value * 8 + (next - b'0') as u32;
                        chars.next();
                    }
                }
                bytes.push(value as u8);
            }
            Some(other) => bytes.push(other),
            None => {}
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Pairs each run of removed lines with the added lines that follow it and marks
/// the changed words, so an edited line reads as an edit rather than a rewrite.
pub fn emphasize(lines: &mut [DiffLine]) {
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind != LineKind::Removed {
            i += 1;
            continue;
        }
        let removed_start = i;
        while i < lines.len() && lines[i].kind == LineKind::Removed {
            i += 1;
        }
        let added_start = i;
        while i < lines.len() && lines[i].kind == LineKind::Added {
            i += 1;
        }
        let pairs = (added_start - removed_start).min(i - added_start);
        for k in 0..pairs {
            let (old, new) = (removed_start + k, added_start + k);
            if let Some((old_ranges, new_ranges)) = word_changes(&lines[old].text, &lines[new].text) {
                lines[old].emphasis = old_ranges;
                lines[new].emphasis = new_ranges;
            }
        }
    }
}

type Ranges = Vec<Range<usize>>;

fn word_changes(old: &str, new: &str) -> Option<(Ranges, Ranges)> {
    if old.len() > 2000 || new.len() > 2000 {
        return None;
    }
    let (old_tokens, new_tokens) = (tokens(old), tokens(new));
    let old_words: Vec<&str> = old_tokens.iter().map(|r| &old[r.clone()]).collect();
    let new_words: Vec<&str> = new_tokens.iter().map(|r| &new[r.clone()]).collect();
    let ops = capture_diff_slices(Algorithm::Myers, &old_words, &new_words);
    // Mostly rewritten lines are clearer without emphasis.
    let same: usize = ops
        .iter()
        .map(|op| match op {
            DiffOp::Equal { old_index, len, .. } => {
                old_tokens[*old_index..old_index + len].iter().map(|r| r.len()).sum()
            }
            _ => 0,
        })
        .sum();
    if (2 * same) as f32 / ((old.len() + new.len()).max(1) as f32) < 0.4 {
        return None;
    }
    let (mut old_ranges, mut new_ranges): (Ranges, Ranges) = (Vec::new(), Vec::new());
    let span = |tokens: &[Range<usize>], index: usize, len: usize| tokens[index].start..tokens[index + len - 1].end;
    for op in ops {
        match op {
            DiffOp::Equal { .. } => {}
            DiffOp::Delete { old_index, old_len, .. } => {
                push_range(&mut old_ranges, span(&old_tokens, old_index, old_len))
            }
            DiffOp::Insert { new_index, new_len, .. } => {
                push_range(&mut new_ranges, span(&new_tokens, new_index, new_len))
            }
            DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                push_range(&mut old_ranges, span(&old_tokens, old_index, old_len));
                push_range(&mut new_ranges, span(&new_tokens, new_index, new_len));
            }
        }
    }
    Some((old_ranges, new_ranges))
}

/// Code-aware tokens: identifier/number runs, whitespace runs, and single symbols,
/// so `compute(1)` → `compute`, `(`, `1`, `)`.
fn tokens(text: &str) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    let class = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            0
        } else if c.is_whitespace() {
            1
        } else {
            2
        }
    };
    let mut prev: Option<u8> = None;
    for (i, c) in text.char_indices() {
        let k = class(c);
        match out.last_mut() {
            Some(last) if prev == Some(k) && k != 2 => last.end = i + c.len_utf8(),
            _ => out.push(i..i + c.len_utf8()),
        }
        prev = Some(k);
    }
    out
}

fn push_range(ranges: &mut Ranges, range: Range<usize>) {
    match ranges.last_mut() {
        Some(last) if last.end == range.start => last.end = range.end,
        _ => ranges.push(range),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,4 +1,5 @@ mod tests
 fn main() {
-    let value = compute(1);
+    let value = compute(2);
+    println!(\"{value}\");
 }
\\ No newline at end of file
diff --git a/new file.txt b/new file.txt
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/new file.txt
@@ -0,0 +1 @@
+hello
diff --git a/gone.rs b/gone.rs
deleted file mode 100644
--- a/gone.rs
+++ /dev/null
@@ -1 +0,0 @@
-bye
diff --git a/old/name.rs b/new/name.rs
similarity index 90%
rename from old/name.rs
rename to new/name.rs
diff --git a/logo.png b/logo.png
index 4444444..5555555 100644
Binary files a/logo.png and b/logo.png differ
";

    #[test]
    fn parses_statuses_paths_and_line_numbers() {
        let files = parse_unified(SAMPLE);
        assert_eq!(files.len(), 5);

        let lib = &files[0];
        assert_eq!(lib.status, FileStatus::Modified);
        assert_eq!(lib.path(), "src/lib.rs");
        assert_eq!((lib.additions(), lib.deletions()), (2, 1));
        let lines = &lib.hunks[0].lines;
        assert_eq!(lines[0].old_no, Some(1));
        assert_eq!(lines[1].kind, LineKind::Removed);
        assert_eq!(lines[1].old_no, Some(2));
        assert_eq!(lines[2].new_no, Some(2));
        assert_eq!(lines[3].new_no, Some(3));
        assert_eq!(lines[4].new_no, Some(4));
        assert_eq!(lines[5].kind, LineKind::Note);

        assert_eq!(files[1].status, FileStatus::Added);
        assert_eq!(files[1].path(), "new file.txt");
        assert_eq!(files[1].old_path, None);

        assert_eq!(files[2].status, FileStatus::Deleted);
        assert_eq!(files[2].path(), "gone.rs");
        assert_eq!(files[2].new_path, None);

        assert_eq!(files[3].status, FileStatus::Renamed);
        assert_eq!(files[3].old_path.as_deref(), Some("old/name.rs"));
        assert_eq!(files[3].new_path.as_deref(), Some("new/name.rs"));

        assert!(files[4].binary);
        assert_eq!(files[4].path(), "logo.png");
    }

    #[test]
    fn edited_line_gets_word_emphasis() {
        let files = parse_unified(SAMPLE);
        let lines = &files[0].hunks[0].lines;
        let removed = &lines[1];
        let added = &lines[2];
        assert_eq!(&removed.text[removed.emphasis[0].clone()], "1");
        assert_eq!(&added.text[added.emphasis[0].clone()], "2");
        assert!(lines[3].emphasis.is_empty());
    }

    #[test]
    fn unquotes_escaped_paths() {
        assert_eq!(unquote("\"caf\\303\\251.txt\""), "café.txt");
        assert_eq!(unquote("plain.txt"), "plain.txt");
    }

    #[test]
    fn untracked_file_is_all_added() {
        let file = FileDiff::untracked("notes.md".into(), "a\nb\n");
        assert_eq!(file.status, FileStatus::Untracked);
        assert_eq!(file.additions(), 2);
        assert_eq!(file.hunks[0].lines[1].new_no, Some(2));
    }
}
