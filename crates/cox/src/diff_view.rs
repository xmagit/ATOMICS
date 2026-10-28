//! The diff panel: a background worker that diffs a worktree against the merge-base
//! with `base_ref`, and a virtualized view where clicking a line starts a review
//! comment that can be sent to the agent working in that worktree.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use cox_core::diff::{FileStatus, LineKind, WorktreeDiff};
use cox_core::files::{self, Dir};
use cox_core::steer::ReviewComment;
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, Key, Rect, RichText, Sense, Stroke, pos2, vec2};

use crate::highlight::{Highlighter, Spans};
use crate::theme::{Palette, lerp};

pub struct Prepared {
    pub diff: WorktreeDiff,
    /// Per file, per hunk, per line.
    pub highlights: Vec<Vec<Vec<Spans>>>,
    pub longest_line: usize,
    /// Every file git shows in the worktree, for the file picker.
    pub all_files: Vec<String>,
}

/// A whole file opened from the file picker.
pub struct FileView {
    pub lines: Vec<String>,
    pub highlights: Vec<Spans>,
    pub longest_line: usize,
    /// Why the content is not shown (binary, too large).
    pub note: Option<String>,
}

pub enum WorkerResult {
    Diff { root: PathBuf, result: Result<Arc<Prepared>, String> },
    File { root: PathBuf, path: String, result: Result<Arc<FileView>, String> },
}

enum Job {
    Diff { root: PathBuf, base_ref: String, highlighter: Arc<Highlighter> },
    File { root: PathBuf, path: String, highlighter: Arc<Highlighter> },
}

pub struct DiffWorker {
    tx: Sender<Job>,
    pub rx: Receiver<WorkerResult>,
}

const MAX_VIEW_BYTES: u64 = 4 * 1024 * 1024;
const MAX_HIGHLIGHT_LINES: usize = 20_000;

impl DiffWorker {
    pub fn new(ctx: egui::Context) -> DiffWorker {
        let (tx, jobs) = channel::<Job>();
        let (results, rx) = channel();
        std::thread::Builder::new()
            .name("cox-diff".into())
            .spawn(move || {
                while let Ok(first) = jobs.recv() {
                    // Coalesce bursts (an agent saving many files) into one job per worktree and file.
                    let mut diffs: HashMap<PathBuf, Job> = HashMap::new();
                    let mut files: HashMap<(PathBuf, String), Job> = HashMap::new();
                    for job in std::iter::once(first).chain(std::iter::from_fn(|| jobs.try_recv().ok())) {
                        match &job {
                            Job::Diff { root, .. } => diffs.insert(root.clone(), job),
                            Job::File { root, path, .. } => files.insert((root.clone(), path.clone()), job),
                        };
                    }
                    for job in diffs.into_values().chain(files.into_values()) {
                        if results.send(run(job)).is_err() {
                            return;
                        }
                        ctx.request_repaint();
                    }
                }
            })
            .expect("spawn diff worker");
        DiffWorker { tx, rx }
    }

    pub fn request(&self, root: PathBuf, base_ref: String, highlighter: Arc<Highlighter>) {
        let _ = self.tx.send(Job::Diff { root, base_ref, highlighter });
    }

    pub fn request_file(&self, root: PathBuf, path: String, highlighter: Arc<Highlighter>) {
        let _ = self.tx.send(Job::File { root, path, highlighter });
    }
}

fn run(job: Job) -> WorkerResult {
    match job {
        Job::Diff { root, base_ref, highlighter } => {
            let result = prepare(&root, &base_ref, &highlighter).map(Arc::new).map_err(|e| format!("{e:#}"));
            WorkerResult::Diff { root, result }
        }
        Job::File { root, path, highlighter } => {
            let result = load_file(&root, &path, &highlighter).map(Arc::new).map_err(|e| format!("{e:#}"));
            WorkerResult::File { root, path, result }
        }
    }
}

fn load_file(root: &std::path::Path, path: &str, highlighter: &Highlighter) -> anyhow::Result<FileView> {
    let full = root.join(path);
    let size = std::fs::metadata(&full)?.len();
    let empty = |note: &str| FileView {
        lines: Vec::new(),
        highlights: Vec::new(),
        longest_line: 0,
        note: Some(note.to_string()),
    };
    if size > MAX_VIEW_BYTES {
        return Ok(empty("File is too large to show."));
    }
    let bytes = std::fs::read(&full)?;
    if bytes[..bytes.len().min(8000)].contains(&0) {
        return Ok(empty("Binary file."));
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let highlights = if lines.len() <= MAX_HIGHLIGHT_LINES {
        highlighter.lines(path, &lines)
    } else {
        vec![Vec::new(); lines.len()]
    };
    let longest_line = lines.iter().map(|l| l.chars().count() + l.matches('\t').count() * 3).max().unwrap_or(0);
    Ok(FileView { lines, highlights, longest_line, note: None })
}

fn prepare(root: &std::path::Path, base_ref: &str, highlighter: &Highlighter) -> anyhow::Result<Prepared> {
    let diff = cox_core::git::diff_against_base(root, base_ref)?;
    let all_files = cox_core::git::list_files(root).unwrap_or_default();
    let highlights = diff.files.iter().map(|f| highlighter.file(f)).collect();
    let longest_line = diff
        .files
        .iter()
        .flat_map(|f| f.hunks.iter().flat_map(|h| h.lines.iter()))
        .map(|l| l.text.chars().count() + l.text.matches('\t').count() * 3)
        .chain(diff.files.iter().map(|f| f.path().len() + 20))
        .max()
        .unwrap_or(0);
    Ok(Prepared { diff, highlights, longest_line, all_files })
}

#[derive(Default)]
pub struct DiffState {
    pub prepared: Option<Arc<Prepared>>,
    pub error: Option<String>,
    pub loading: bool,
    /// A refresh is wanted; it is sent once things have been quiet for a moment.
    pub dirty_since: Option<Instant>,
    pub last_request: Option<Instant>,
    pub collapsed: HashSet<String>,
    pub comments: Vec<ReviewComment>,
    editor: Option<Editor>,
    scroll_to: Option<f32>,
    /// A whole file shown instead of the diff.
    pub viewing: Option<String>,
    pub file: Option<Arc<FileView>>,
    pub file_error: Option<String>,
    /// The file last opened from the changed-files list or the explorer.
    pub focused: Option<String>,
}

/// The explorer of one project: the main checkout's files, with the changes of the
/// worktree under review laid over them.
#[derive(Default)]
pub struct ExplorerState {
    pub filter: String,
    pub changed_only: bool,
    pub open_dirs: HashSet<String>,
    pub show_worktrees: bool,
    tree: Option<((usize, usize), Arc<Dir>)>,
    seeded_for: usize,
}

impl ExplorerState {
    fn tree(&mut self, main: &Arc<Prepared>, target: &Arc<Prepared>) -> Arc<Dir> {
        let key = (Arc::as_ptr(main) as usize, Arc::as_ptr(target) as usize);
        if let Some((k, tree)) = &self.tree
            && *k == key
        {
            return tree.clone();
        }
        // The main checkout's own untracked files are clutter, unless it tracks nothing yet.
        let same = key.0 == key.1 && !main.all_files.is_empty();
        let changed: HashSet<&str> = target
            .diff
            .files
            .iter()
            .filter(|f| !(same && f.status == FileStatus::Untracked))
            .map(|f| f.path())
            .collect();
        let mut all = main.all_files.clone();
        all.extend(changed.iter().map(|p| p.to_string()));
        all.sort();
        all.dedup();
        // Opening another worktree unfolds the folders it changed.
        if self.seeded_for != key.1 && key.0 != key.1 {
            self.seeded_for = key.1;
            for path in &changed {
                self.open_dirs.extend(files::ancestors(path).map(str::to_string));
            }
        }
        let tree = Arc::new(files::build(&all, &changed));
        self.tree = Some((key, tree.clone()));
        tree
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Anchor {
    Diff(usize, usize, usize),
    File(usize),
}

struct Editor {
    anchor: Anchor,
    text: String,
    focus: bool,
}

pub enum DiffAction {
    None,
    SendReview,
    Refresh,
    Close,
    /// Open the file in the editor, at a line when one was double-clicked.
    Edit {
        path: Option<String>,
        line: Option<u32>,
    },
}

const DEBOUNCE: Duration = Duration::from_millis(350);

impl DiffState {
    /// Opens `path`: its hunks when the diff touches it, the whole file otherwise.
    /// Returns true when the file content has to be loaded.
    pub fn open(&mut self, path: &str) -> bool {
        self.editor = None;
        if let Some(prepared) = &self.prepared
            && let Some(index) = prepared.diff.files.iter().position(|f| f.path() == path)
        {
            self.viewing = None;
            self.collapsed.remove(path);
            self.scroll_to = Some(index as f32);
            self.focused = Some(path.to_string());
            return false;
        }
        if self.viewing.as_deref() != Some(path) {
            self.file = None;
        }
        self.viewing = Some(path.to_string());
        self.focused = Some(path.to_string());
        self.file_error = None;
        true
    }

    /// Moves to the next (`+1`) or previous (`-1`) changed file and jumps there.
    /// Returns the file and the first line it changes.
    pub fn step(&mut self, delta: i32) -> Option<(String, Option<u32>)> {
        let prepared = self.prepared.clone()?;
        let files = &prepared.diff.files;
        if files.is_empty() {
            return None;
        }
        let current = self.focused.as_deref().and_then(|f| files.iter().position(|x| x.path() == f));
        let next = match current {
            Some(i) => (i as i32 + delta).clamp(0, files.len() as i32 - 1) as usize,
            None if delta < 0 => files.len() - 1,
            None => 0,
        };
        let file = &files[next];
        let path = file.path().to_string();
        self.open(&path);
        Some((path, first_change(file)))
    }

    pub fn mark_dirty(&mut self) {
        self.dirty_since.get_or_insert_with(Instant::now);
    }

    /// True when a dirty diff has been quiet long enough to recompute.
    pub fn due(&mut self) -> bool {
        match self.dirty_since {
            Some(since) if since.elapsed() >= DEBOUNCE => {
                self.dirty_since = None;
                self.loading = true;
                self.last_request = Some(Instant::now());
                true
            }
            _ => false,
        }
    }
}

#[derive(Clone, Copy)]
enum Row {
    File(usize),
    Binary,
    Hunk(usize, usize),
    Line(usize, usize, usize),
    Comment(usize),
    Editor,
    FileLine(usize),
}

pub struct Header<'a> {
    pub title: &'a str,
    pub branch: Option<&'a str>,
    pub can_send: bool,
}

pub fn show(
    ui: &mut egui::Ui,
    rect: Rect,
    state: &mut DiffState,
    header: Header<'_>,
    palette: &Palette,
    font: &FontId,
) -> DiffAction {
    let mut action = DiffAction::None;
    ui.painter().rect_filled(rect, 0.0, palette.background);
    let mut panel = ui.new_child(egui::UiBuilder::new().id_salt("diff-panel").max_rect(rect.shrink2(vec2(10.0, 6.0))));
    let ui = &mut panel;

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(header.title).strong().size(14.0).color(palette.foreground));
        if let Some(branch) = header.branch {
            ui.label(egui::RichText::new(branch).color(palette.muted).monospace());
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("×").on_hover_text("Close diff").clicked() {
                action = DiffAction::Close;
            }
            if ui.small_button("⟳").on_hover_text("Refresh").clicked() {
                action = DiffAction::Refresh;
            }
            if state.viewing.is_some() && ui.small_button("← diff").on_hover_text("Back to the diff").clicked() {
                state.viewing = None;
            }
            if ui
                .small_button("editor")
                .on_hover_text("Edit in the editor (cmd+e) · double-click a line to edit there")
                .clicked()
            {
                let first = state.prepared.as_ref().and_then(|p| p.diff.files.first().map(|f| f.path().to_string()));
                action = DiffAction::Edit { path: state.viewing.clone().or(first), line: None };
            }
            let n = state.comments.len();
            let send = egui::Button::new(egui::RichText::new(format!("Send review ({n})")).color(if n > 0 {
                Color32::WHITE
            } else {
                palette.muted
            }))
            .fill(if n > 0 { palette.accent.gamma_multiply(0.55) } else { Color32::TRANSPARENT });
            if ui
                .add_enabled(n > 0 && header.can_send, send)
                .on_hover_text("Send every comment to the agent as one message")
                .on_disabled_hover_text(if header.can_send {
                    "Click a line to comment"
                } else {
                    "No agent in this worktree"
                })
                .clicked()
            {
                action = DiffAction::SendReview;
            }
            if !state.comments.is_empty() && ui.small_button("discard").clicked() {
                state.comments.clear();
            }
        });
    });

    if let Some(path) = state.viewing.clone() {
        if let Some(line) = show_file(ui, state, &path, palette, font) {
            action = DiffAction::Edit { path: Some(path), line: Some(line) };
        }
        return action;
    }

    let Some(prepared) = state.prepared.clone() else {
        ui.add_space(20.0);
        let text = match (&state.error, state.loading) {
            (Some(err), _) => format!("Could not diff: {err}"),
            (None, true) => "Diffing against main…".to_string(),
            (None, false) => "Nothing selected".to_string(),
        };
        ui.label(egui::RichText::new(text).color(palette.muted));
        return action;
    };
    let diff = &prepared.diff;

    ui.horizontal(|ui| {
        let short = &diff.base[..diff.base.len().min(8)];
        ui.label(egui::RichText::new(format!("{} files", diff.files.len())).color(palette.foreground));
        ui.label(egui::RichText::new(format!("+{}", diff.additions())).color(palette.success));
        ui.label(egui::RichText::new(format!("−{}", diff.deletions())).color(palette.error));
        ui.label(egui::RichText::new(format!("vs {} @ {short}", diff.base_ref)).color(palette.muted).monospace());
        if state.loading {
            ui.spinner();
        }
        if let Some(err) = &state.error {
            ui.label(egui::RichText::new("refresh failed").color(palette.error)).on_hover_text(err);
        }
        ui.menu_button("files ▾", |ui| {
            egui::ScrollArea::vertical().id_salt("diff-files").max_height(420.0).show(ui, |ui| {
                for (i, file) in diff.files.iter().enumerate() {
                    let label = format!("{}  +{} −{}", file.path(), file.additions(), file.deletions());
                    if ui.button(label).clicked() {
                        state.collapsed.remove(file.path());
                        state.scroll_to = Some(i as f32);
                        ui.close();
                    }
                }
            });
        });
        if !diff.files.is_empty() {
            let all = state.collapsed.len() == diff.files.len();
            if ui.small_button(if all { "expand all" } else { "collapse all" }).clicked() {
                if all {
                    state.collapsed.clear();
                } else {
                    state.collapsed = diff.files.iter().map(|f| f.path().to_string()).collect();
                }
            }
        }
    });
    ui.add_space(4.0);

    if diff.files.is_empty() {
        ui.add_space(16.0);
        ui.label(egui::RichText::new("No changes against the merge-base yet.").color(palette.muted));
        return action;
    }

    // Comment anchors: (path, line, removed) → comment indices.
    let mut anchored: HashMap<(&str, u32, bool), Vec<usize>> = HashMap::new();
    for (i, c) in state.comments.iter().enumerate() {
        if let Some(line) = c.line {
            anchored.entry((c.path.as_str(), line, c.removed)).or_default().push(i);
        }
    }
    let mut rows: Vec<Row> = Vec::new();
    let mut file_rows: Vec<usize> = Vec::new();
    for (fi, file) in diff.files.iter().enumerate() {
        file_rows.push(rows.len());
        rows.push(Row::File(fi));
        if state.collapsed.contains(file.path()) {
            continue;
        }
        if file.binary {
            rows.push(Row::Binary);
            continue;
        }
        for (hi, hunk) in file.hunks.iter().enumerate() {
            rows.push(Row::Hunk(fi, hi));
            for (li, line) in hunk.lines.iter().enumerate() {
                rows.push(Row::Line(fi, hi, li));
                let key = match line.kind {
                    LineKind::Removed => line.old_no.map(|n| (file.path(), n, true)),
                    _ => line.new_no.map(|n| (file.path(), n, false)),
                };
                if let Some(ids) = key.and_then(|k| anchored.get(&k)) {
                    rows.extend(ids.iter().map(|&i| Row::Comment(i)));
                }
                if state.editor.as_ref().is_some_and(|e| e.anchor == Anchor::Diff(fi, hi, li)) {
                    rows.push(Row::Editor);
                }
            }
        }
    }

    let char_w = ui.fonts_mut(|f| f.glyph_width(font, '0'));
    let row_h = ui.fonts_mut(|f| f.row_height(font)) + 3.0;
    let gutter = char_w * 13.0;
    let content_w = (gutter + char_w * (prepared.longest_line as f32 + 2.0)).max(ui.available_width());

    let mut scroll = egui::ScrollArea::both().id_salt(("diff-rows", header.title)).auto_shrink([false, false]);
    if let Some(file) = state.scroll_to.take() {
        let row = file_rows.get(file as usize).copied().unwrap_or(0);
        scroll = scroll.vertical_scroll_offset(row as f32 * row_h);
    }
    let mut clicked_line: Option<(usize, usize, usize)> = None;
    let mut edit_line: Option<(String, Option<u32>)> = None;
    let mut toggle: Option<String> = None;
    let mut delete: Option<usize> = None;
    let mut commit_editor = false;
    let mut cancel_editor = false;

    ui.spacing_mut().item_spacing.y = 0.0;
    scroll.show_rows(ui, row_h, rows.len(), |ui, range| {
        for row in &rows[range] {
            let (rect, response) = ui.allocate_exact_size(vec2(content_w, row_h), Sense::click());
            let painter = ui.painter();
            let text_pos = |x: f32| pos2(rect.left() + x, rect.top() + 1.5);
            match *row {
                Row::File(fi) => {
                    let file = &diff.files[fi];
                    painter.rect_filled(rect, 0.0, palette.panel);
                    if state.focused.as_deref() == Some(file.path()) {
                        painter.rect_filled(rect.with_max_x(rect.left() + 3.0), 0.0, palette.accent);
                    }
                    painter.line_segment(
                        [rect.left_top(), rect.right_top()],
                        Stroke::new(1.0, lerp(palette.panel, palette.muted, 0.3)),
                    );
                    let arrow = if state.collapsed.contains(file.path()) { "▸" } else { "▾" };
                    let (badge, color) = status_badge(file.status, palette);
                    painter.text(text_pos(4.0), egui::Align2::LEFT_TOP, arrow, font.clone(), palette.muted);
                    painter.text(text_pos(char_w * 2.0 + 4.0), egui::Align2::LEFT_TOP, badge, font.clone(), color);
                    let name = match (file.status, &file.old_path) {
                        (FileStatus::Renamed, Some(old)) => format!("{old} → {}", file.path()),
                        _ => file.path().to_string(),
                    };
                    painter.text(
                        text_pos(char_w * 4.0 + 4.0),
                        egui::Align2::LEFT_TOP,
                        &name,
                        font.clone(),
                        palette.foreground,
                    );
                    let counts = format!("+{} −{}", file.additions(), file.deletions());
                    painter.text(
                        text_pos(char_w * (name.chars().count() as f32 + 6.0)),
                        egui::Align2::LEFT_TOP,
                        counts,
                        font.clone(),
                        palette.muted,
                    );
                    if response.clicked() {
                        toggle = Some(file.path().to_string());
                    }
                }
                Row::Binary => {
                    painter.text(
                        text_pos(gutter),
                        egui::Align2::LEFT_TOP,
                        "binary or large file",
                        font.clone(),
                        palette.muted,
                    );
                }
                Row::Hunk(fi, hi) => {
                    let header = &diff.files[fi].hunks[hi].header;
                    painter.rect_filled(rect, 0.0, lerp(palette.background, palette.accent, 0.07));
                    painter.text(
                        text_pos(gutter),
                        egui::Align2::LEFT_TOP,
                        header,
                        font.clone(),
                        lerp(palette.muted, palette.accent, 0.5),
                    );
                }
                Row::Line(fi, hi, li) => {
                    let line = &diff.files[fi].hunks[hi].lines[li];
                    let (bg, emphasis, sign) = match line.kind {
                        LineKind::Added => (palette.added, palette.added_emphasis, "+"),
                        LineKind::Removed => (palette.removed, palette.removed_emphasis, "-"),
                        _ => (palette.background, palette.background, " "),
                    };
                    if bg != palette.background {
                        painter.rect_filled(rect, 0.0, bg);
                    }
                    if response.hovered() {
                        painter.rect_filled(rect, 0.0, Color32::from_white_alpha(8));
                        painter.text(
                            text_pos(char_w * 11.0),
                            egui::Align2::LEFT_TOP,
                            "💬",
                            font.clone(),
                            palette.accent,
                        );
                    }
                    let number = |n: Option<u32>| n.map(|n| format!("{n:>5}")).unwrap_or_default();
                    painter.text(
                        text_pos(0.0),
                        egui::Align2::LEFT_TOP,
                        number(line.old_no),
                        font.clone(),
                        palette.muted,
                    );
                    painter.text(
                        text_pos(char_w * 5.5),
                        egui::Align2::LEFT_TOP,
                        number(line.new_no),
                        font.clone(),
                        palette.muted,
                    );
                    if !response.hovered() {
                        painter.text(
                            text_pos(char_w * 11.5),
                            egui::Align2::LEFT_TOP,
                            sign,
                            font.clone(),
                            palette.muted,
                        );
                    }
                    let spans = prepared.highlights.get(fi).and_then(|h| h.get(hi)).and_then(|h| h.get(li));
                    let job = line_job(
                        &line.text,
                        spans.map(Vec::as_slice).unwrap_or(&[]),
                        &line.emphasis,
                        emphasis,
                        font,
                        palette,
                    );
                    let galley = ui.fonts_mut(|f| f.layout_job(job));
                    ui.painter().galley(text_pos(gutter), galley, palette.foreground);
                    if response.double_clicked() && line.kind != LineKind::Note {
                        let target = if line.kind == LineKind::Removed { None } else { line.new_no };
                        edit_line = Some((diff.files[fi].path().to_string(), target.or(line.old_no)));
                    } else if response.clicked() && line.kind != LineKind::Note {
                        clicked_line = Some((fi, hi, li));
                    }
                }
                Row::Comment(i) => {
                    if comment_row(painter, rect, &response, gutter, char_w, &state.comments[i], font, palette) {
                        delete = Some(i);
                    }
                }
                Row::Editor => {
                    if let Some(editor) = state.editor.as_mut() {
                        let (commit, cancel) = editor_row(ui, rect, gutter, char_w, editor, font);
                        commit_editor |= commit;
                        cancel_editor |= cancel;
                    }
                }
                Row::FileLine(_) => {}
            }
        }
    });

    if let Some(path) = toggle
        && !state.collapsed.remove(&path)
    {
        state.collapsed.insert(path);
    }
    if let Some(i) = delete {
        state.comments.remove(i);
    }
    if commit_editor
        && let Some(editor) = state.editor.take()
        && let Anchor::Diff(fi, hi, li) = editor.anchor
    {
        let file = &diff.files[fi];
        let line = &file.hunks[hi].lines[li];
        let removed = line.kind == LineKind::Removed;
        if !editor.text.trim().is_empty() {
            state.comments.push(ReviewComment {
                path: file.path().to_string(),
                line: if removed { line.old_no } else { line.new_no },
                removed,
                excerpt: line.text.clone(),
                body: editor.text.trim().to_string(),
            });
        }
    } else if cancel_editor {
        state.editor = None;
    }
    if let Some((path, line)) = edit_line {
        state.editor = None;
        return DiffAction::Edit { path: Some(path), line };
    }
    if let Some((fi, hi, li)) = clicked_line {
        state.editor = Some(Editor { anchor: Anchor::Diff(fi, hi, li), text: String::new(), focus: true });
    }
    action
}

/// A whole file, read-only, where clicking a line starts a comment just like in the
/// diff. Returns a line that was double-clicked, to edit there.
fn show_file(ui: &mut egui::Ui, state: &mut DiffState, path: &str, palette: &Palette, font: &FontId) -> Option<u32> {
    ui.horizontal(|ui| {
        ui.label(RichText::new(path).monospace().color(palette.foreground));
        ui.label(RichText::new("unchanged · click a line to comment").color(palette.muted).size(12.0));
    });
    ui.add_space(4.0);
    let Some(file) = state.file.clone() else {
        let text = match &state.file_error {
            Some(err) => format!("Could not open {path}: {err}"),
            None => "Loading…".to_string(),
        };
        ui.label(RichText::new(text).color(palette.muted));
        return None;
    };
    if let Some(note) = &file.note {
        ui.label(RichText::new(note).color(palette.muted));
        return None;
    }

    let mut anchored: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, c) in state.comments.iter().enumerate() {
        if c.path == path
            && !c.removed
            && let Some(line) = c.line
        {
            anchored.entry(line).or_default().push(i);
        }
    }
    let mut rows = Vec::with_capacity(file.lines.len());
    for i in 0..file.lines.len() {
        rows.push(Row::FileLine(i));
        if let Some(ids) = anchored.get(&(i as u32 + 1)) {
            rows.extend(ids.iter().map(|&c| Row::Comment(c)));
        }
        if state.editor.as_ref().is_some_and(|e| e.anchor == Anchor::File(i)) {
            rows.push(Row::Editor);
        }
    }

    let char_w = ui.fonts_mut(|f| f.glyph_width(font, '0'));
    let row_h = ui.fonts_mut(|f| f.row_height(font)) + 3.0;
    let gutter = char_w * 9.0;
    let content_w = (gutter + char_w * (file.longest_line as f32 + 2.0)).max(ui.available_width());
    let mut clicked: Option<usize> = None;
    let mut double_clicked: Option<usize> = None;
    let mut delete: Option<usize> = None;
    let (mut commit, mut cancel) = (false, false);

    ui.spacing_mut().item_spacing.y = 0.0;
    egui::ScrollArea::both().id_salt(("file-rows", path)).auto_shrink([false, false]).show_rows(
        ui,
        row_h,
        rows.len(),
        |ui, range| {
            for row in &rows[range] {
                let (rect, response) = ui.allocate_exact_size(vec2(content_w, row_h), Sense::click());
                let painter = ui.painter();
                let text_pos = |x: f32| pos2(rect.left() + x, rect.top() + 1.5);
                match *row {
                    Row::FileLine(i) => {
                        if response.hovered() {
                            painter.rect_filled(rect, 0.0, Color32::from_white_alpha(8));
                            painter.text(
                                text_pos(char_w * 7.0),
                                egui::Align2::LEFT_TOP,
                                "💬",
                                font.clone(),
                                palette.accent,
                            );
                        }
                        let number = format!("{:>6}", i + 1);
                        painter.text(text_pos(0.0), egui::Align2::LEFT_TOP, number, font.clone(), palette.muted);
                        let spans = file.highlights.get(i).map(Vec::as_slice).unwrap_or(&[]);
                        let job = line_job(&file.lines[i], spans, &[], palette.background, font, palette);
                        let galley = ui.fonts_mut(|f| f.layout_job(job));
                        ui.painter().galley(text_pos(gutter), galley, palette.foreground);
                        if response.double_clicked() {
                            double_clicked = Some(i);
                        } else if response.clicked() {
                            clicked = Some(i);
                        }
                    }
                    Row::Comment(i) => {
                        if comment_row(painter, rect, &response, gutter, char_w, &state.comments[i], font, palette) {
                            delete = Some(i);
                        }
                    }
                    Row::Editor => {
                        if let Some(editor) = state.editor.as_mut() {
                            let (c, x) = editor_row(ui, rect, gutter, char_w, editor, font);
                            commit |= c;
                            cancel |= x;
                        }
                    }
                    _ => {}
                }
            }
        },
    );

    if let Some(i) = delete {
        state.comments.remove(i);
    }
    if commit
        && let Some(editor) = state.editor.take()
        && let Anchor::File(i) = editor.anchor
        && !editor.text.trim().is_empty()
    {
        state.comments.push(ReviewComment {
            path: path.to_string(),
            line: Some(i as u32 + 1),
            removed: false,
            excerpt: file.lines[i].clone(),
            body: editor.text.trim().to_string(),
        });
    } else if cancel {
        state.editor = None;
    }
    if let Some(i) = double_clicked {
        state.editor = None;
        return Some(i as u32 + 1);
    }
    if let Some(i) = clicked {
        state.editor = Some(Editor { anchor: Anchor::File(i), text: String::new(), focus: true });
    }
    None
}

/// Draws a pending comment under its line. Returns true when clicked (to delete it).
#[allow(clippy::too_many_arguments)]
fn comment_row(
    painter: &egui::Painter,
    rect: Rect,
    response: &egui::Response,
    gutter: f32,
    char_w: f32,
    comment: &ReviewComment,
    font: &FontId,
    palette: &Palette,
) -> bool {
    let band = rect.with_min_x(rect.left() + gutter - char_w);
    painter.rect_filled(band, 3.0, lerp(palette.panel, palette.accent, 0.18));
    let text = format!("» {}", comment.body.replace('\n', " "));
    painter.text(
        pos2(rect.left() + gutter, rect.top() + 1.5),
        egui::Align2::LEFT_TOP,
        text,
        font.clone(),
        palette.foreground,
    );
    if response.hovered() {
        let pos = pos2(rect.left() + gutter - char_w * 3.0, rect.top() + 1.5);
        painter.text(pos, egui::Align2::LEFT_TOP, "×", font.clone(), palette.error);
    }
    response.clicked()
}

/// The inline comment input. Returns (commit, cancel).
fn editor_row(
    ui: &mut egui::Ui,
    rect: Rect,
    gutter: f32,
    char_w: f32,
    editor: &mut Editor,
    font: &FontId,
) -> (bool, bool) {
    let field = rect.with_min_x(rect.left() + gutter - char_w);
    let mut child = ui.new_child(egui::UiBuilder::new().id_salt("comment-editor").max_rect(field));
    let edit = egui::TextEdit::singleline(&mut editor.text)
        .hint_text("Comment for the agent · Enter adds · Esc cancels")
        .font(font.clone())
        .desired_width(field.width().min(900.0));
    let response = child.add(edit);
    if editor.focus {
        response.request_focus();
        editor.focus = false;
    }
    if !response.lost_focus() {
        return (false, false);
    }
    if child.input(|i| i.key_pressed(Key::Enter)) {
        (true, false)
    } else {
        (false, child.input(|i| i.key_pressed(Key::Escape)) || editor.text.trim().is_empty())
    }
}

enum TreeRow {
    Dir { path: String, name: String, depth: usize, open: bool, changed: usize },
    File { path: String, depth: usize, full: bool },
}

fn flatten(dir: &Dir, prefix: &str, depth: usize, open: &HashSet<String>, rows: &mut Vec<TreeRow>) {
    for (name, child) in &dir.dirs {
        let path = format!("{prefix}{name}");
        let is_open = open.contains(&path);
        rows.push(TreeRow::Dir {
            path: path.clone(),
            name: name.clone(),
            depth,
            open: is_open,
            changed: child.changed,
        });
        if is_open {
            flatten(child, &format!("{path}/"), depth + 1, open, rows);
        }
    }
    for name in &dir.files {
        rows.push(TreeRow::File { path: format!("{prefix}{name}"), depth, full: false });
    }
}

/// The explorer's file tree: `main`'s files with `target`'s changes. Returns the file clicked.
pub fn files_panel(
    ui: &mut egui::Ui,
    state: &mut ExplorerState,
    main: &Arc<Prepared>,
    target: &Arc<Prepared>,
    viewing: Option<&str>,
    palette: &Palette,
) -> Option<String> {
    ui.horizontal(|ui| {
        let width = (ui.available_width() - 74.0).max(60.0);
        ui.add(egui::TextEdit::singleline(&mut state.filter).hint_text("Filter files").desired_width(width));
        ui.toggle_value(&mut state.changed_only, "changed").on_hover_text("Only files in the diff");
    });
    let tree = state.tree(main, target);
    let prepared = target.clone();
    let changed: HashMap<&str, (FileStatus, usize, usize)> =
        prepared.diff.files.iter().map(|f| (f.path(), (f.status, f.additions(), f.deletions()))).collect();

    let filter = state.filter.trim().to_string();
    let rows: Vec<TreeRow> = if !filter.is_empty() {
        let mut all = main.all_files.clone();
        all.extend(changed.keys().map(|p| p.to_string()));
        all.sort();
        all.dedup();
        files::best_matches(&filter, &all, 400)
            .into_iter()
            .map(|p| TreeRow::File { path: p.to_string(), depth: 0, full: true })
            .collect()
    } else if state.changed_only {
        prepared.diff.files.iter().map(|f| TreeRow::File { path: f.path().to_string(), depth: 0, full: true }).collect()
    } else {
        let mut rows = Vec::new();
        flatten(&tree, "", 0, &state.open_dirs, &mut rows);
        rows
    };

    let font = FontId::proportional(12.5);
    let mono = FontId::monospace(10.5);
    let row_h = 20.0;
    let mut clicked = None;
    let mut toggle = None;
    egui::ScrollArea::vertical().id_salt("file-tree").auto_shrink([false, false]).show_rows(
        ui,
        row_h,
        rows.len(),
        |ui, range| {
            for row in &rows[range] {
                let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                let painter = ui.painter_at(rect);
                let y = rect.center().y;
                match row {
                    TreeRow::Dir { path, name, depth, open, changed } => {
                        if response.hovered() {
                            painter.rect_filled(rect, 4.0, lerp(palette.panel, palette.foreground, 0.05));
                        }
                        let x = rect.left() + 4.0 + *depth as f32 * 12.0;
                        let arrow = if *open { "▾" } else { "▸" };
                        painter.text(pos2(x, y), egui::Align2::LEFT_CENTER, arrow, font.clone(), palette.muted);
                        painter.text(
                            pos2(x + 12.0, y),
                            egui::Align2::LEFT_CENTER,
                            name,
                            font.clone(),
                            palette.foreground,
                        );
                        if *changed > 0 {
                            let text = changed.to_string();
                            painter.text(
                                rect.right_center() - vec2(6.0, 0.0),
                                egui::Align2::RIGHT_CENTER,
                                text,
                                mono.clone(),
                                palette.warning,
                            );
                        }
                        if response.clicked() {
                            toggle = Some(path.clone());
                        }
                    }
                    TreeRow::File { path, depth, full } => {
                        let viewing = viewing == Some(path.as_str());
                        if viewing {
                            painter.rect_filled(rect, 4.0, lerp(palette.panel, palette.accent, 0.22));
                        } else if response.hovered() {
                            painter.rect_filled(rect, 4.0, lerp(palette.panel, palette.foreground, 0.05));
                        }
                        let x = rect.left() + 16.0 + *depth as f32 * 12.0;
                        let status = changed.get(path.as_str());
                        let color = match status {
                            Some((status, ..)) => status_badge(*status, palette).1,
                            None => lerp(palette.muted, palette.foreground, 0.55),
                        };
                        let (dir, name) = match path.rfind('/') {
                            Some(i) if *full => (&path[..=i], &path[i + 1..]),
                            Some(i) => ("", &path[i + 1..]),
                            None => ("", path.as_str()),
                        };
                        let mut job = LayoutJob::default();
                        let format = |color| TextFormat { font_id: font.clone(), color, ..Default::default() };
                        job.append(dir, 0.0, format(palette.muted));
                        job.append(name, 0.0, format(color));
                        let galley = ui.fonts_mut(|f| f.layout_job(job));
                        painter.galley(pos2(x, y - galley.size().y / 2.0), galley, color);
                        if let Some((status, add, del)) = status {
                            let text = match status {
                                FileStatus::Deleted => "deleted".to_string(),
                                _ => format!("+{add} −{del}"),
                            };
                            let right = rect.right_center() - vec2(6.0, 0.0);
                            painter.text(right, egui::Align2::RIGHT_CENTER, text, mono.clone(), palette.muted);
                        }
                        if response.clicked() {
                            clicked = Some(path.clone());
                        }
                        response.on_hover_text(path.as_str());
                    }
                }
            }
        },
    );
    if let Some(path) = toggle
        && !state.open_dirs.remove(&path)
    {
        state.open_dirs.insert(path);
    }
    clicked
}

fn status_badge(status: FileStatus, palette: &Palette) -> (&'static str, Color32) {
    match status {
        FileStatus::Added => ("A", palette.success),
        FileStatus::Untracked => ("U", palette.success),
        FileStatus::Deleted => ("D", palette.error),
        FileStatus::Renamed => ("R", palette.warning),
        FileStatus::Modified => ("M", palette.info),
    }
}

/// Lays out one code line: syntax colors, plus a background on changed words.
fn line_job(
    text: &str,
    spans: &[(std::ops::Range<usize>, Color32)],
    emphasis: &[std::ops::Range<usize>],
    emphasis_bg: Color32,
    font: &FontId,
    palette: &Palette,
) -> LayoutJob {
    let mut cuts: Vec<usize> = vec![0, text.len()];
    for (range, _) in spans {
        cuts.extend([range.start, range.end]);
    }
    for range in emphasis {
        cuts.extend([range.start, range.end]);
    }
    cuts.retain(|&c| c <= text.len() && text.is_char_boundary(c));
    cuts.sort_unstable();
    cuts.dedup();

    let mut job = LayoutJob::default();
    for pair in cuts.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        if start == end {
            continue;
        }
        let color =
            spans.iter().find(|(r, _)| r.start <= start && end <= r.end).map(|(_, c)| *c).unwrap_or(palette.foreground);
        let background =
            if emphasis.iter().any(|r| r.start <= start && end <= r.end) { emphasis_bg } else { Color32::TRANSPARENT };
        let piece = text[start..end].replace('\t', "    ");
        job.append(&piece, 0.0, TextFormat { font_id: font.clone(), color, background, ..Default::default() });
    }
    job
}

/// The changed files of the worktree under review, beside the diff or editor.
/// Returns the clicked file and the first line it changes.
pub fn changed_files(
    ui: &mut egui::Ui,
    rect: Rect,
    state: &DiffState,
    current: Option<&str>,
    palette: &Palette,
) -> Option<(String, Option<u32>)> {
    ui.painter().rect_filled(rect, 0.0, palette.panel);
    ui.painter().line_segment([rect.right_top(), rect.right_bottom()], Stroke::new(1.0, palette.border));
    let mut child =
        ui.new_child(egui::UiBuilder::new().id_salt("changed-files").max_rect(rect.shrink2(vec2(6.0, 6.0))));
    let ui = &mut child;
    let Some(prepared) = state.prepared.as_ref() else {
        ui.label(RichText::new("…").color(palette.muted));
        return None;
    };
    let diff = &prepared.diff;
    ui.horizontal(|ui| {
        ui.label(RichText::new("CHANGED").color(palette.accent).strong().size(11.0));
        ui.label(RichText::new(format!("{}", diff.files.len())).color(palette.muted).size(11.0));
        ui.label(RichText::new(format!("+{}", diff.additions())).color(palette.success).size(11.0));
        ui.label(RichText::new(format!("−{}", diff.deletions())).color(palette.error).size(11.0));
    });
    ui.add_space(4.0);
    if diff.files.is_empty() {
        ui.label(RichText::new("No changes yet").color(palette.muted).size(12.0));
        return None;
    }
    let font = palette.ui(12.0);
    let mono = FontId::monospace(10.0);
    let row_h = 34.0;
    let mut clicked = None;
    egui::ScrollArea::vertical().id_salt("changed-files-scroll").auto_shrink([false, false]).show_rows(
        ui,
        row_h,
        diff.files.len(),
        |ui, range| {
            for file in &diff.files[range] {
                let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                let painter = ui.painter_at(rect);
                let selected = current == Some(file.path());
                if selected {
                    painter.rect_filled(rect, 2.0, lerp(palette.panel, palette.accent, 0.16));
                    painter.rect_filled(rect.with_max_x(rect.left() + 2.0), 0.0, palette.accent);
                } else if response.hovered() {
                    painter.rect_filled(rect, 2.0, palette.raised);
                }
                let (badge, color) = status_badge(file.status, palette);
                let path = file.path();
                let (dir, name) = match path.rfind('/') {
                    Some(i) => (&path[..i], &path[i + 1..]),
                    None => ("", path),
                };
                painter.text(rect.left_top() + vec2(6.0, 4.0), egui::Align2::LEFT_TOP, badge, mono.clone(), color);
                let name_color = if selected { palette.accent } else { palette.foreground };
                painter.text(rect.left_top() + vec2(20.0, 3.0), egui::Align2::LEFT_TOP, name, font.clone(), name_color);
                painter.text(
                    rect.left_top() + vec2(20.0, 19.0),
                    egui::Align2::LEFT_TOP,
                    dir,
                    mono.clone(),
                    palette.muted,
                );
                let counts = if file.binary {
                    "bin".to_string()
                } else {
                    format!("+{} −{}", file.additions(), file.deletions())
                };
                painter.text(
                    rect.right_top() + vec2(-4.0, 4.0),
                    egui::Align2::RIGHT_TOP,
                    counts,
                    mono.clone(),
                    palette.muted,
                );
                if response.clicked() {
                    clicked = Some((path.to_string(), first_change(file)));
                }
                response.on_hover_text(path);
            }
        },
    );
    clicked
}

/// The first line a file's change touches in the new version.
fn first_change(file: &cox_core::diff::FileDiff) -> Option<u32> {
    file.hunks
        .iter()
        .flat_map(|h| &h.lines)
        .find(|l| l.kind == LineKind::Added)
        .and_then(|l| l.new_no)
        .or_else(|| file.hunks.first().map(|h| h.new_start.max(1)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cox_core::diff::FileDiff;

    fn state_with(paths: &[&str]) -> DiffState {
        let files = paths.iter().map(|p| FileDiff::untracked(p.to_string(), "a\nb\n")).collect();
        let diff = WorktreeDiff { base_ref: "main".into(), base: "abc".into(), files };
        let prepared = Prepared { diff, highlights: Vec::new(), longest_line: 1, all_files: Vec::new() };
        DiffState { prepared: Some(Arc::new(prepared)), ..DiffState::default() }
    }

    #[test]
    fn shift_arrows_walk_changed_files_and_stop_at_the_ends() {
        let mut state = state_with(&["a.rs", "b.rs", "c.rs"]);
        assert_eq!(state.step(1).unwrap(), ("a.rs".to_string(), Some(1)));
        assert_eq!(state.step(1).unwrap().0, "b.rs");
        assert_eq!(state.step(1).unwrap().0, "c.rs");
        assert_eq!(state.step(1).unwrap().0, "c.rs");
        assert_eq!(state.step(-1).unwrap().0, "b.rs");
        assert_eq!(state.focused.as_deref(), Some("b.rs"));
        assert!(state.viewing.is_none());
        assert!(DiffState::default().step(1).is_none());
    }
}
