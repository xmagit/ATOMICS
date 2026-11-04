//! The diff panel: a background worker that diffs a worktree against the merge-base
//! with `base_ref`, and a virtualized view where clicking a line starts a review
//! comment that can be sent to the agent working in that worktree.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use atomics_core::diff::{DiffLine, FileStatus, LineKind, SplitRow, split_rows};
use atomics_core::diff_data::Span;
use atomics_core::files::{self, Dir};
use atomics_core::layout::DiffSplit;
use atomics_core::steer::ReviewComment;
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, Key, Pos2, Rect, RichText, Sense, Stroke, pos2, vec2};

use crate::keys::shortcut_label;
use crate::theme::{Palette, lerp};
use crate::touch::{self, Touch};

pub use atomics_core::diff_data::{FileView, Prepared};

#[derive(Default)]
pub struct DiffState {
    pub prepared: Option<Arc<Prepared>>,
    pub error: Option<String>,
    pub loading: bool,
    pub collapsed: HashSet<String>,
    pub comments: Vec<ReviewComment>,
    editor: Option<Editor>,
    scroll_to: Option<ScrollTarget>,
    hunk: Option<usize>,
    /// The line at the top of the diff and whether it was side by side, to keep the place
    /// when that changes.
    top: Option<((usize, usize, usize), bool)>,
    /// A whole file shown instead of the diff.
    pub viewing: Option<String>,
    pub file: Option<Arc<FileView>>,
    pub file_error: Option<String>,
    /// The file last opened or reached by scrolling the diff.
    pub focused: Option<String>,
    /// Long lines run on to the right, to scroll to, where a phone would wrap them.
    pub unwrapped: bool,
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
        let mut all = main.all_files.to_vec();
        all.extend(changed.iter().map(|&p| Arc::from(p)));
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

enum ScrollTarget {
    File(usize),
    Hunk(usize, usize),
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
    /// Side by side (true) or unified, picked with the header toggle.
    Split(bool),
}

impl DiffState {
    /// Opens the comment box on `comment`'s line again, with its text, for a comment that did not go through; it
    /// takes the keyboard with `focus`. False, and nothing changes, when another comment is being written or the
    /// diff no longer has that line.
    pub fn reopen_comment(&mut self, comment: &ReviewComment, focus: bool) -> bool {
        if self.editor.as_ref().is_some_and(|editor| !editor.text.trim().is_empty()) {
            return false;
        }
        let Some(prepared) = &self.prepared else { return false };
        let on_it = |line: &DiffLine| {
            let number = if comment.removed { line.old_no } else { line.new_no };
            number.is_some() && number == comment.line && (line.kind == LineKind::Removed) == comment.removed
        };
        let files = prepared.diff.files.iter().enumerate().filter(|(_, file)| file.path() == comment.path);
        let mut anchors = files.flat_map(|(fi, file)| {
            let hunks = file.hunks.iter().enumerate();
            hunks.filter_map(move |(hi, hunk)| hunk.lines.iter().position(on_it).map(|li| Anchor::Diff(fi, hi, li)))
        });
        let Some(anchor) = anchors.next() else { return false };
        self.editor = Some(Editor { anchor, text: comment.body.clone(), focus });
        true
    }

    /// Opens `path`: its hunks when the diff touches it, the whole file otherwise.
    /// Returns true when the file content has to be loaded.
    pub fn open(&mut self, path: &str) -> bool {
        let whole_file =
            !self.prepared.as_ref().is_some_and(|prepared| prepared.diff.files.iter().any(|file| file.path() == path));
        self.open_resolved(path, whole_file);
        whole_file
    }

    /// Apply the host's authoritative target even if a newer diff arrived while
    /// its client event was queued. A diff that no longer contains the file is
    /// simply the newest diff; it must not become an unsubscribed file load.
    pub fn open_resolved(&mut self, path: &str, whole_file: bool) {
        self.editor = None;
        self.focused = Some(path.to_string());
        self.hunk = None;
        if !whole_file {
            self.viewing = None;
            self.collapsed.remove(path);
            self.scroll_to = self
                .prepared
                .as_ref()
                .and_then(|prepared| prepared.diff.files.iter().position(|file| file.path() == path))
                .map(ScrollTarget::File);
            return;
        }
        if self.viewing.as_deref() != Some(path) {
            self.file = None;
        }
        self.viewing = Some(path.to_string());
        self.file_error = None;
    }

    /// Moves to the next (`+1`) or previous (`-1`) changed file and jumps to its header.
    /// Returns the file and the first line it changes.
    pub fn step(&mut self, delta: i32) -> Option<(String, Option<u32>)> {
        let prepared = self.prepared.clone()?;
        let files = &prepared.diff.files;
        if files.is_empty() {
            return None;
        }
        let current = self.focused.as_deref().and_then(|path| files.iter().position(|file| file.path() == path));
        let next = crate::panel_nav::step_index(current, files.len(), delta)?;
        let file = &files[next];
        let path = file.path().to_string();
        self.open(&path);
        Some((path, first_change(file)))
    }

    /// Moves to the next (`+1`) or previous (`-1`) hunk, crossing file boundaries.
    /// Files without hunks still have one stop at their header.
    /// Returns the file and the first line the selected hunk changes.
    pub fn step_hunk(&mut self, delta: i32) -> Option<(String, Option<u32>)> {
        let prepared = self.prepared.clone()?;
        let files = &prepared.diff.files;
        if files.is_empty() {
            return None;
        }
        let sections: Vec<_> = files
            .iter()
            .enumerate()
            .flat_map(|(fi, file)| (0..file.hunks.len().max(1)).map(move |hi| (fi, hi)))
            .collect();
        let current = self.focused.as_deref().and_then(|path| {
            let fi = files.iter().position(|file| file.path() == path)?;
            let hi = self.hunk.unwrap_or(0).min(files[fi].hunks.len().saturating_sub(1));
            sections.iter().position(|&section| section == (fi, hi))
        });
        let next = match current {
            // A file header precedes its first hunk.
            Some(i) if self.hunk.is_none() && delta > 0 => i,
            _ => crate::panel_nav::step_index(current, sections.len(), delta)?,
        };
        let (fi, hi) = sections[next];
        let file = &files[fi];
        let path = file.path().to_string();
        self.open(&path);
        self.hunk = Some(hi);
        let line = file.hunks.get(hi).map(|hunk| {
            hunk.lines
                .iter()
                .find(|line| line.kind == LineKind::Added)
                .and_then(|line| line.new_no)
                .unwrap_or(hunk.new_start.max(1))
        });
        self.scroll_to = Some(if file.hunks.is_empty() { ScrollTarget::File(fi) } else { ScrollTarget::Hunk(fi, hi) });
        Some((path, line))
    }
}

#[derive(Clone, Copy)]
enum Row {
    File(usize),
    Binary,
    Hunk(usize, usize),
    Line(usize, usize, usize),
    /// On a phone, a long line's continuation (the first is 1), wrapped under the line ([`wrapping`]).
    Wrap(usize, usize, usize, usize),
    /// Side by side: the old and the new line of one hunk row.
    Pair(usize, usize, SplitRow),
    Comment(usize),
    Editor,
    FileLine(usize),
}

impl Row {
    /// The line a diff row shows: the new side of a pair, unless that is blank.
    fn line(&self) -> Option<(usize, usize, usize)> {
        match *self {
            Row::Line(fi, hi, li) | Row::Wrap(fi, hi, li, _) => Some((fi, hi, li)),
            Row::Pair(fi, hi, pair) => pair.new.or(pair.old).map(|li| (fi, hi, li)),
            _ => None,
        }
    }

    fn shows(&self, (fi, hi, li): (usize, usize, usize)) -> bool {
        match *self {
            Row::Line(f, h, l) => (f, h, l) == (fi, hi, li),
            Row::Pair(f, h, pair) => (f, h) == (fi, hi) && (pair.old == Some(li) || pair.new == Some(li)),
            _ => false,
        }
    }
}

pub struct Header<'a> {
    pub title: &'a str,
    pub branch: Option<&'a str>,
    pub can_send: bool,
    pub split: DiffSplit,
    pub editor_shortcut: &'a str,
    /// A phone: the header stacks, a side shows at a time and long lines wrap ([`crate::touch`]).
    pub touch: Option<Touch>,
    /// A click on a line starts a review comment, a click on one deletes it, and Send review shows. Without it a
    /// comment box already open keeps its draft: Enter waits until it can go.
    pub review: bool,
    /// A double click on a line, and the editor button, open the file in the editor.
    pub edit: bool,
}

pub use crate::panel_nav::focus_id;
use crate::panel_nav::keyboard_scroll;

/// The keys that scroll the focused diff. PageUp/PageDown are not among them: they walk the
/// hunks (see the shortcuts).
const SCROLL_KEYS: [Key; 4] = [Key::ArrowUp, Key::ArrowDown, Key::Home, Key::End];

pub fn show(
    ui: &mut egui::Ui,
    rect: Rect,
    state: &mut DiffState,
    header: Header<'_>,
    palette: &Palette,
    font: &FontId,
) -> DiffAction {
    crate::panel_nav::focusable(ui, rect);
    let mut action = DiffAction::None;
    let touch = header.touch;
    let mut split = header.split.split(rect.width()) && touch.is_none();
    ui.painter().rect_filled(rect, 0.0, palette.background);
    let margin = touch.map_or(vec2(10.0, 6.0), |touch| vec2(touch.gutter, 6.0));
    let mut panel = ui.new_child(egui::UiBuilder::new().id_salt("diff-panel").max_rect(rect.shrink2(margin)));
    let ui = &mut panel;

    if touch.is_none() {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(header.title).strong().size(14.0).color(palette.foreground));
            if let Some(branch) = header.branch {
                ui.label(egui::RichText::new(branch).color(palette.muted).monospace());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                header_buttons(ui, state, &header, palette, &mut action);
            });
        });
    } else {
        // A phone stacks the header: the title, the branch and counts, then every button in a row that wraps,
        // with Close last.
        let title = egui::RichText::new(header.title).strong().size(15.0).color(palette.foreground);
        ui.add(egui::Label::new(title).wrap());
        let prepared = state.prepared.clone().filter(|_| state.viewing.is_none());
        ui.horizontal_wrapped(|ui| {
            if let Some(branch) = header.branch {
                ui.label(egui::RichText::new(branch).color(palette.muted).monospace());
            }
            if let Some(prepared) = &prepared {
                counts(ui, prepared, state.error.as_deref(), palette);
            }
        });
        ui.horizontal_wrapped(|ui| {
            header_buttons(ui, state, &header, palette, &mut action);
            if let Some(prepared) = &prepared {
                diff_buttons(ui, state, &prepared.diff, &mut split, touch, &mut action);
            }
            close_button(ui, touch, &mut action);
        });
    }

    if let Some(path) = state.viewing.clone() {
        if let Some(line) = show_file(ui, state, &path, palette, font, header.review, header.edit) {
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

    if touch.is_none() {
        ui.horizontal(|ui| {
            counts(ui, &prepared, state.error.as_deref(), palette);
            diff_buttons(ui, state, diff, &mut split, None, &mut action);
        });
    }
    ui.add_space(4.0);

    if diff.files.is_empty() {
        ui.add_space(16.0);
        ui.label(egui::RichText::new("No changes against the merge-base yet.").color(palette.muted));
        return action;
    }

    let char_w = ui.fonts_mut(|f| f.glyph_width(font, '0'));
    let row_h = ui.fonts_mut(|f| f.row_height(font)) + 3.0;
    let view = ui.available_rect_before_wrap();
    // A phone numbers each line once, as wide as the diff's highest number, and wraps long lines to the screen
    // unless the user turned that off.
    let digits = touch.map(|_| {
        let lines = diff.files.iter().flat_map(|f| &f.hunks).flat_map(|h| &h.lines);
        let highest = lines.filter_map(|l| l.new_no.max(l.old_no)).max().unwrap_or(1);
        highest.to_string().len().max(2)
    });
    let gutter = digits.map_or(char_w * 13.0, |digits| char_w * (digits as f32 + 3.0));
    // The width a wrapped row has for code, in points; a row holds at least eight digits.
    let wrap = (touch.is_some() && !state.unwrapped && !split).then(|| (view.width() - gutter).max(char_w * 8.0));
    // How a line wraps there, measured with the glyphs it is drawn with.
    let ctx = ui.ctx().clone();
    let wrapped = |text: &str, width: f32| ctx.fonts_mut(|f| wrapping(text, width, |c| f.glyph_width(font, c)));

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
            // A line's comments and the open editor go right under it.
            let annotate = |rows: &mut Vec<Row>, li: usize| {
                let line = &hunk.lines[li];
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
            };
            if split {
                for pair in split_rows(&hunk.lines) {
                    rows.push(Row::Pair(fi, hi, pair));
                    for li in pair.old.into_iter().chain(pair.new.filter(|&new| pair.old != Some(new))) {
                        annotate(&mut rows, li);
                    }
                }
            } else {
                for li in 0..hunk.lines.len() {
                    rows.push(Row::Line(fi, hi, li));
                    if let Some(width) = wrap {
                        let more = wrapped(&hunk.lines[li].text, width).starts.len();
                        rows.extend((1..=more).map(|piece| Row::Wrap(fi, hi, li, piece)));
                    }
                    annotate(&mut rows, li);
                }
            }
        }
    }

    let code_w = char_w * (prepared.longest_line as f32 + 2.0);
    // Side by side, the line numbers stay put and the code of both halves scrolls together.
    let mid = view.left() + (view.width() / 2.0).floor();
    let side_gutter = char_w * 8.0;
    let content_w = if split {
        view.width() + (side_gutter + code_w - (mid - view.left())).max(0.0)
    } else if wrap.is_some() {
        view.width()
    } else {
        (gutter + code_w).max(view.width())
    };
    let head_x = if split { side_gutter } else { gutter };
    let number = |n: Option<u32>| n.map(|n| format!("{n:>5}")).unwrap_or_default();
    let spans = |fi: usize, hi: usize, li: usize| -> &[Span] {
        prepared.highlights.get(fi).and_then(|h| h.get(hi)).and_then(|h| h.get(li)).map(Vec::as_slice).unwrap_or(&[])
    };

    let mut scroll = egui::ScrollArea::both().id_salt(("diff-rows", header.title)).auto_shrink([false, false]);
    let jump = match state.scroll_to.take() {
        Some(ScrollTarget::File(file)) => Some(file_rows.get(file).copied().unwrap_or(0)),
        Some(ScrollTarget::Hunk(fi, hi)) => {
            rows.iter().position(|row| matches!(*row, Row::Hunk(f, h) if (f, h) == (fi, hi)))
        }
        None => {
            state.top.filter(|&(_, was)| was != split).and_then(|(line, _)| rows.iter().position(|r| r.shows(line)))
        }
    };
    let scroll_id = ui.make_persistent_id(egui::IdSalt::new(("diff-rows", header.title)));
    let before = egui::scroll_area::State::load(ui.ctx(), scroll_id).map(|s| s.clamped_offset().y).unwrap_or_default();
    if let Some(offset) =
        keyboard_scroll(ui, scroll_id, row_h, 0..rows.len(), &SCROLL_KEYS, jump.map(|row| row as f32 * row_h))
    {
        scroll = scroll.vertical_scroll_offset(offset);
    }
    let mut clicked_line: Option<(usize, usize, usize)> = None;
    let mut edit_line: Option<(String, Option<u32>)> = None;
    let (can_review, can_edit) = (header.review, header.edit);
    let mut toggle: Option<String> = None;
    let mut delete: Option<usize> = None;
    let mut commit_editor = false;
    let mut cancel_editor = false;

    ui.spacing_mut().item_spacing.y = 0.0;
    let shown = scroll.show_rows(ui, row_h, rows.len(), |ui, range| {
        let top = rows[range.clone()].iter().find_map(Row::line);
        for row in &rows[range] {
            let (rect, response) = ui.allocate_exact_size(vec2(content_w, row_h), Sense::click());
            let painter = ui.painter();
            let text_pos = |x: f32| pos2(rect.left() + x, rect.top() + 1.5);
            let side = |right: bool| {
                if right {
                    rect.with_min_x(mid).with_max_x(view.right())
                } else {
                    rect.with_min_x(view.left()).with_max_x(mid)
                }
            };
            match *row {
                Row::File(fi) => {
                    let file = &diff.files[fi];
                    painter.rect_filled(rect, 0.0, palette.panel);
                    if state.focused.as_deref() == Some(file.path()) {
                        painter.rect_filled(rect.with_max_x(rect.left() + 3.0), 0.0, palette.accent);
                    }
                    painter.line_segment([rect.left_top(), rect.right_top()], Stroke::new(1.0, palette.separator));
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
                        text_pos(head_x),
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
                        text_pos(head_x),
                        egui::Align2::LEFT_TOP,
                        header,
                        font.clone(),
                        lerp(palette.muted, palette.accent, 0.5),
                    );
                }
                Row::Line(fi, hi, li) | Row::Wrap(fi, hi, li, _) => {
                    let line = &diff.files[fi].hunks[hi].lines[li];
                    let code_x = rect.left() + gutter;
                    let hovered = response.hovered();
                    let piece = if let Row::Wrap(.., piece) = *row { piece } else { 0 };
                    let segment = wrap.map(|width| Piece::of(&wrapped(&line.text, width), piece, width));
                    let spans = spans(fi, hi, li);
                    let hovered = hovered && can_review;
                    paint_line(painter, rect, code_x, 0.0, char_w, line, spans, hovered, font, palette, segment);
                    if let Some(digits) = digits.filter(|_| piece == 0) {
                        let n = if line.kind == LineKind::Removed { line.old_no } else { line.new_no };
                        let n = n.map(|n| format!("{n:>digits$}")).unwrap_or_default();
                        painter.text(text_pos(0.0), egui::Align2::LEFT_TOP, n, font.clone(), palette.quiet);
                    } else if digits.is_none() {
                        painter.text(
                            text_pos(0.0),
                            egui::Align2::LEFT_TOP,
                            number(line.old_no),
                            font.clone(),
                            palette.quiet,
                        );
                        painter.text(
                            text_pos(char_w * 5.5),
                            egui::Align2::LEFT_TOP,
                            number(line.new_no),
                            font.clone(),
                            palette.quiet,
                        );
                    }
                    if line.kind != LineKind::Note {
                        if can_edit && response.double_clicked() {
                            edit_line = Some((diff.files[fi].path().to_string(), edit_target(line)));
                        } else if can_review && response.clicked() {
                            clicked_line = Some((fi, hi, li));
                        }
                    }
                }
                Row::Pair(fi, hi, pair) => {
                    let lines = &diff.files[fi].hunks[hi].lines;
                    let shift = view.left() - rect.left();
                    let hovered = response.hover_pos().map(|p| p.x >= mid);
                    for (right, li) in [(false, pair.old), (true, pair.new)] {
                        let cell = side(right);
                        let Some(li) = li else {
                            painter.rect_filled(cell, 0.0, palette.panel);
                            continue;
                        };
                        let line = &lines[li];
                        let code_x = cell.left() + side_gutter;
                        let over = hovered == Some(right) && can_review;
                        let spans = spans(fi, hi, li);
                        paint_line(painter, cell, code_x, shift, char_w, line, spans, over, font, palette, None);
                        let n = if right { line.new_no } else { line.old_no };
                        let at = pos2(cell.left() + char_w * 0.5, rect.top() + 1.5);
                        painter.text(at, egui::Align2::LEFT_TOP, number(n), font.clone(), palette.quiet);
                    }
                    let clicked = response.interact_pointer_pos().map(|p| p.x >= mid);
                    let target = clicked
                        .and_then(|right| if right { pair.new } else { pair.old })
                        .filter(|&li| lines[li].kind != LineKind::Note);
                    if let Some(li) = target {
                        if can_edit && response.double_clicked() {
                            edit_line = Some((diff.files[fi].path().to_string(), edit_target(&lines[li])));
                        } else if can_review && response.clicked() {
                            clicked_line = Some((fi, hi, li));
                        }
                    }
                }
                Row::Comment(i) => {
                    let comment = &state.comments[i];
                    let (cell, gutter) = if split { (side(!comment.removed), side_gutter) } else { (rect, gutter) };
                    let over = |pos: Option<Pos2>| pos.is_some_and(|p| cell.x_range().contains(p.x));
                    let hovered = can_review && over(response.hover_pos());
                    comment_row(&painter.with_clip_rect(cell), cell, hovered, gutter, char_w, comment, font, palette);
                    if can_review && response.clicked() && over(response.interact_pointer_pos()) {
                        delete = Some(i);
                    }
                }
                Row::Editor => {
                    if let Some(editor) = state.editor.as_mut() {
                        let (cell, gutter) = match editor.anchor {
                            Anchor::Diff(fi, hi, li) if split => {
                                let removed = diff.files[fi].hunks[hi].lines[li].kind == LineKind::Removed;
                                (side(!removed), side_gutter)
                            }
                            _ => (rect, gutter),
                        };
                        let (commit, cancel) = editor_row(ui, cell, gutter, char_w, editor, font, can_review);
                        commit_editor |= commit;
                        cancel_editor |= cancel;
                    }
                }
                Row::FileLine(_) => {}
            }
            if split && matches!(row, Row::Pair(..) | Row::Comment(_) | Row::Editor) {
                ui.painter().vline(mid, rect.y_range(), Stroke::new(1.0, palette.border));
            }
        }
        top
    });
    state.top = shown.inner.map(|line| (line, split));
    if jump.is_none() && shown.state.clamped_offset().y != before {
        // Use the viewport's top row, rather than the virtualizer's extra visible lines.
        let top = (shown.state.clamped_offset().y / row_h) as usize;
        if let Some((fi, hi)) = rows[..=top.min(rows.len() - 1)].iter().rev().find_map(|row| match *row {
            Row::File(fi) => Some((fi, None)),
            Row::Hunk(fi, hi) => Some((fi, Some(hi))),
            _ => row.line().map(|(fi, hi, _)| (fi, Some(hi))),
        }) {
            state.focused = Some(diff.files[fi].path().to_string());
            state.hunk = hi;
        }
    }

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

/// The header's buttons, in the order a right-to-left row shows them from its end: Close, Refresh, Back to
/// the diff, Edit, Send review and Discard.
fn header_buttons(
    ui: &mut egui::Ui,
    state: &mut DiffState,
    header: &Header<'_>,
    palette: &Palette,
    action: &mut DiffAction,
) {
    let touch = header.touch;
    // Right to left on a desktop, so Close is the header's last button; a phone draws it after the rest.
    if touch.is_none() {
        close_button(ui, touch, action);
    }
    if touch::small_button(ui, touch, "⟳").on_hover_text("Refresh").clicked() {
        *action = DiffAction::Refresh;
    }
    if state.viewing.is_some() && touch::small_button(ui, touch, "← diff").on_hover_text("Back to the diff").clicked()
    {
        state.viewing = None;
    }
    if header.edit
        && touch::small_button(ui, touch, "editor")
            .on_hover_text(format!(
                "Edit in the editor ({}) · double-click a line to edit there",
                shortcut_label(ui.ctx(), header.editor_shortcut)
            ))
            .clicked()
    {
        let first = state.prepared.as_ref().and_then(|p| p.diff.files.first().map(|f| f.path().to_string()));
        *action = DiffAction::Edit { path: state.viewing.clone().or(first), line: None };
    }
    if !header.review {
        return;
    }
    let n = state.comments.len();
    let (text, fill) = if n > 0 { (palette.on_accent, palette.accent) } else { (palette.muted, Color32::TRANSPARENT) };
    let send = egui::Button::new(egui::RichText::new(format!("Send review ({n})")).color(text)).fill(fill);
    if ui
        .add_enabled(n > 0 && header.can_send, touch::sized(send, touch))
        .on_hover_text("Send every comment to the agent as one message")
        .on_disabled_hover_text(if header.can_send { "Click a line to comment" } else { "No agent in this worktree" })
        .clicked()
    {
        *action = DiffAction::SendReview;
    }
    if !state.comments.is_empty() && touch::small_button(ui, touch, "discard").clicked() {
        state.comments.clear();
    }
}

fn close_button(ui: &mut egui::Ui, touch: Option<Touch>, action: &mut DiffAction) {
    if touch::small_button(ui, touch, "×").on_hover_text("Close diff").clicked() {
        *action = DiffAction::Close;
    }
}

/// What the diff holds: its files, the lines it adds and removes, and what it is against.
fn counts(ui: &mut egui::Ui, prepared: &Prepared, error: Option<&str>, palette: &Palette) {
    let diff = &prepared.diff;
    let short = &diff.base[..diff.base.len().min(8)];
    ui.label(egui::RichText::new(format!("{} files", diff.files.len())).color(palette.foreground));
    ui.label(egui::RichText::new(format!("+{}", prepared.additions)).color(palette.success));
    ui.label(egui::RichText::new(format!("−{}", prepared.deletions)).color(palette.error));
    ui.label(egui::RichText::new(format!("vs {} @ {short}", diff.base_ref)).color(palette.muted).monospace());
    if let Some(err) = error {
        ui.label(egui::RichText::new("refresh failed").color(palette.error)).on_hover_text(err);
    }
}

/// The diff's own buttons: its files, to jump to one, folding them all, and side by side; on a phone,
/// wrapping long lines instead.
fn diff_buttons(
    ui: &mut egui::Ui,
    state: &mut DiffState,
    diff: &atomics_core::diff::WorktreeDiff,
    split: &mut bool,
    touch: Option<Touch>,
    action: &mut DiffAction,
) {
    ui.menu_button("files ▾", |ui| {
        egui::ScrollArea::vertical().id_salt("diff-files").max_height(420.0).show(ui, |ui| {
            for (i, file) in diff.files.iter().enumerate() {
                let label = format!("{}  +{} −{}", file.path(), file.additions(), file.deletions());
                if ui.button(label).clicked() {
                    state.collapsed.remove(file.path());
                    state.scroll_to = Some(ScrollTarget::File(i));
                    ui.close();
                }
            }
        });
    });
    if !diff.files.is_empty() {
        let all = state.collapsed.len() == diff.files.len();
        if touch::small_button(ui, touch, if all { "expand all" } else { "collapse all" }).clicked() {
            if all {
                state.collapsed.clear();
            } else {
                state.collapsed = diff.files.iter().map(|f| f.path().to_string()).collect();
            }
        }
    }
    if touch.is_some() {
        let wrap = egui::Button::selectable(!state.unwrapped, "wrap");
        if ui.add(touch::sized(wrap, touch)).on_hover_text("Wrap long lines to the screen").clicked() {
            state.unwrapped = !state.unwrapped;
        }
    } else if ui
        .toggle_value(split, "split")
        .on_hover_text("Old and new side by side until ATOMICS quits (diff_split in the config sets the default)")
        .changed()
    {
        *action = DiffAction::Split(*split);
    }
}

/// A whole file, read-only, where clicking a line starts a comment just like in the diff, with `review`.
/// Returns a line that was double-clicked, to edit there, with `edit`.
fn show_file(
    ui: &mut egui::Ui,
    state: &mut DiffState,
    path: &str,
    palette: &Palette,
    font: &FontId,
    review: bool,
    edit: bool,
) -> Option<u32> {
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
    let mut scroll = egui::ScrollArea::both().id_salt(("file-rows", path)).auto_shrink([false, false]);
    let scroll_id = ui.make_persistent_id(egui::IdSalt::new(("file-rows", path)));
    if let Some(offset) = keyboard_scroll(ui, scroll_id, row_h, 0..rows.len(), &SCROLL_KEYS, None) {
        scroll = scroll.vertical_scroll_offset(offset);
    }
    scroll.show_rows(ui, row_h, rows.len(), |ui, range| {
        for row in &rows[range] {
            let (rect, response) = ui.allocate_exact_size(vec2(content_w, row_h), Sense::click());
            let painter = ui.painter();
            let text_pos = |x: f32| pos2(rect.left() + x, rect.top() + 1.5);
            match *row {
                Row::FileLine(i) => {
                    if response.hovered() && review {
                        painter.rect_filled(rect, 0.0, palette.hover());
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
                    if edit && response.double_clicked() {
                        double_clicked = Some(i);
                    } else if review && response.clicked() {
                        clicked = Some(i);
                    }
                }
                Row::Comment(i) => {
                    let comment = &state.comments[i];
                    comment_row(painter, rect, review && response.hovered(), gutter, char_w, comment, font, palette);
                    if review && response.clicked() {
                        delete = Some(i);
                    }
                }
                Row::Editor => {
                    if let Some(editor) = state.editor.as_mut() {
                        let (c, x) = editor_row(ui, rect, gutter, char_w, editor, font, review);
                        commit |= c;
                        cancel |= x;
                    }
                }
                _ => {}
            }
        }
    });

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

/// Draws a pending comment under its line, with a × when hovered (a click deletes it).
#[allow(clippy::too_many_arguments)]
fn comment_row(
    painter: &egui::Painter,
    rect: Rect,
    hovered: bool,
    gutter: f32,
    char_w: f32,
    comment: &ReviewComment,
    font: &FontId,
    palette: &Palette,
) {
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
    if hovered {
        let pos = pos2(rect.left() + gutter - char_w * 3.0, rect.top() + 1.5);
        painter.text(pos, egui::Align2::LEFT_TOP, "×", font.clone(), palette.error);
    }
}

/// The inline comment input. Returns (commit, cancel). Without `review` Enter does nothing: the box keeps its draft
/// and the keyboard. Its id is its own, so that it keeps the keyboard when what is drawn before it changes.
fn editor_row(
    ui: &mut egui::Ui,
    rect: Rect,
    gutter: f32,
    char_w: f32,
    editor: &mut Editor,
    font: &FontId,
    review: bool,
) -> (bool, bool) {
    let field = rect.with_min_x(rect.left() + gutter - char_w);
    let mut child = ui.new_child(egui::UiBuilder::new().id_salt("comment-editor").max_rect(field));
    let edit = egui::TextEdit::singleline(&mut editor.text)
        .id(child.scope_id().with("comment-box"))
        .hint_text("Comment for the agent · Enter adds · Esc cancels")
        .font(font.clone())
        .desired_width(field.width().min(900.0));
    let edit = if review { edit } else { edit.return_key(None::<egui::KeyboardShortcut>) };
    let response = child.add(edit);
    if editor.focus {
        response.request_focus();
        editor.focus = false;
    }
    if !response.lost_focus() {
        return (false, false);
    }
    if child.input(|i| i.key_pressed(Key::Enter)) {
        (review, false)
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
        let mut all = main.all_files.to_vec();
        all.extend(changed.keys().map(|&p| Arc::from(p)));
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

/// How a line wraps in rows of a width, in points: what its continuations hang in by, and where each starts.
#[derive(Debug, Clone, PartialEq)]
struct Wrapped {
    /// The line's own indent, up to half a row.
    indent: f32,
    /// Where each continuation starts, from the line's start: always at a character, never inside one.
    starts: Vec<f32>,
}

/// How a line of `text` wraps in rows `width` points wide, `glyph` giving each character's advance (a tab
/// is four spaces, as the line is drawn). Wide characters (CJK, emoji) take what their glyphs take, so a row
/// never runs past the screen or cuts a character in two.
fn wrapping(text: &str, width: f32, mut glyph: impl FnMut(char) -> f32) -> Wrapped {
    let mut advance = |c: char| if c == '\t' { 4.0 * glyph(' ') } else { glyph(c) };
    let mut indent = 0.0;
    let mut starts = Vec::new();
    let (mut x, mut row_start, mut row_width, mut leading) = (0.0, 0.0, width, true);
    for c in text.chars() {
        let w = advance(c);
        leading &= c.is_whitespace();
        if leading {
            indent = (indent + w).min(width / 2.0);
        }
        if x > row_start && x + w - row_start > row_width + 0.01 {
            starts.push(x);
            row_start = x;
            row_width = width - indent;
        }
        x += w;
    }
    Wrapped { indent, starts }
}

/// The part of a wrapped line one row shows, in points: the code from `start` on, drawn `indent` right of
/// the code column and `width` wide.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Piece {
    start: f32,
    indent: f32,
    width: f32,
}

impl Piece {
    /// The `piece`th part of a line wrapped in rows `width` points wide: the first from the line's start,
    /// each continuation under the line's indent, each up to where the next starts.
    fn of(wrapped: &Wrapped, piece: usize, width: f32) -> Piece {
        let start = if piece == 0 { 0.0 } else { wrapped.starts.get(piece - 1).copied().unwrap_or(f32::INFINITY) };
        let indent = if piece == 0 { 0.0 } else { wrapped.indent };
        let end = wrapped.starts.get(piece).copied().unwrap_or(start + width - indent);
        Piece { start, indent, width: end - start }
    }
}

/// Paints a diff line over `cell`: the background of its kind, the sign (💬 when hovered)
/// left of `code_x`, and the highlighted code from `code_x`, scrolled left by `shift`
/// and clipped to the code column. A wrapped line's `piece` shows that part alone, the sign
/// with the first.
#[allow(clippy::too_many_arguments)]
fn paint_line(
    painter: &egui::Painter,
    cell: Rect,
    code_x: f32,
    shift: f32,
    char_w: f32,
    line: &DiffLine,
    spans: &[Span],
    hovered: bool,
    font: &FontId,
    palette: &Palette,
    piece: Option<Piece>,
) {
    let (bg, emphasis, sign) = match line.kind {
        LineKind::Added => (palette.added, palette.added_emphasis, "+"),
        LineKind::Removed => (palette.removed, palette.removed_emphasis, "-"),
        _ => (palette.background, palette.background, " "),
    };
    if bg != palette.background {
        painter.rect_filled(cell, 0.0, bg);
    }
    let y = cell.top() + 1.5;
    let piece = piece.unwrap_or(Piece { start: 0.0, indent: 0.0, width: f32::INFINITY });
    let first = piece.start == 0.0;
    if hovered {
        painter.rect_filled(cell, 0.0, palette.hover());
    }
    if hovered && first {
        painter.text(pos2(code_x - char_w * 2.0, y), egui::Align2::LEFT_TOP, "💬", font.clone(), palette.accent);
    } else if first {
        painter.text(pos2(code_x - char_w * 1.5, y), egui::Align2::LEFT_TOP, sign, font.clone(), palette.muted);
    }
    let galley = painter.layout_job(line_job(&line.text, spans, &line.emphasis, emphasis, font, palette));
    // A continuation is cut exactly where it starts, so nothing of the part before shows.
    let x = code_x + piece.indent;
    let clip = cell.with_min_x(if first { code_x - 1.0 } else { x }).with_max_x(cell.right().min(x + piece.width));
    painter.with_clip_rect(clip).galley(pos2(x - shift - piece.start, y), galley, palette.foreground);
}

/// Where double-clicking a line opens the editor: the line in the new version, or
/// where a removed line used to be.
fn edit_target(line: &DiffLine) -> Option<u32> {
    let new = if line.kind == LineKind::Removed { None } else { line.new_no };
    new.or(line.old_no)
}

/// Lays out one code line: syntax colors, plus a background on changed words.
fn line_job(
    text: &str,
    spans: &[Span],
    emphasis: &[std::ops::Range<usize>],
    emphasis_bg: Color32,
    font: &FontId,
    palette: &Palette,
) -> LayoutJob {
    let mut cuts: Vec<usize> = vec![0, text.len()];
    for span in spans {
        cuts.extend([span.start as usize, span.end as usize]);
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
            spans.iter().find(|s| s.start as usize <= start && end <= s.end as usize).map_or(palette.foreground, |s| {
                Color32::from_rgba_unmultiplied(s.color[0], s.color[1], s.color[2], s.color[3])
            });
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
        ui.label(RichText::new(format!("+{}", prepared.additions)).color(palette.success).size(11.0));
        ui.label(RichText::new(format!("−{}", prepared.deletions)).color(palette.error).size(11.0));
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
fn first_change(file: &atomics_core::diff::FileDiff) -> Option<u32> {
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
    use atomics_core::diff::{FileDiff, WorktreeDiff};

    fn state_with(paths: &[&str]) -> DiffState {
        let files = paths.iter().map(|p| FileDiff::untracked(p.to_string(), "a\nb\n")).collect();
        let diff = WorktreeDiff { base_ref: "main".into(), base: "abc".into(), files, partial: false };
        let prepared = Prepared {
            diff,
            highlights: Vec::new(),
            longest_line: 1,
            additions: 0,
            deletions: 0,
            all_files: Arc::default(),
        };
        DiffState { prepared: Some(Arc::new(prepared)), ..DiffState::default() }
    }

    #[test]
    fn a_comment_box_keeps_the_keyboard_and_its_draft_until_the_comment_can_go() {
        let ctx = egui::Context::default();
        let mut state = state_with(&["a.rs"]);
        state.editor = Some(Editor { anchor: Anchor::Diff(0, 0, 0), text: "draft".into(), focus: true });
        let palette = crate::Client::default().palette;
        let font = palette.mono(13.0);
        let enter = egui::Event::Key {
            key: Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        let mut time = 0.0;
        let mut frame = |state: &mut DiffState, ahead: usize, review: bool, events: Vec<egui::Event>| {
            time += 0.05;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0))),
                time: Some(time),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                for _ in 0..ahead {
                    ui.label("drawn ahead of the diff");
                }
                let header = Header {
                    title: "title",
                    branch: None,
                    can_send: true,
                    split: DiffSplit::Never,
                    editor_shortcut: "",
                    touch: None,
                    review,
                    edit: false,
                };
                let rect = ui.available_rect_before_wrap();
                show(ui, rect, state, header, &palette, &font);
            });
            output.textures_delta.clear();
            ctx.memory(|m| m.focused().is_some())
        };
        frame(&mut state, 0, true, Vec::new());
        assert!(frame(&mut state, 0, true, Vec::new()), "the box has the keyboard");

        // What comes before it changes, as a line under the frame does when it goes.
        frame(&mut state, 1, true, Vec::new());
        assert!(frame(&mut state, 1, true, Vec::new()), "and it keeps it");

        // Nowhere to send it: Enter does nothing, not even for a frame.
        assert!(frame(&mut state, 0, false, vec![enter.clone()]));
        assert!(frame(&mut state, 0, false, Vec::new()));
        assert_eq!(state.editor.as_ref().map(|editor| editor.text.as_str()), Some("draft"));
        assert!(state.comments.is_empty());

        // Back, one Enter adds it.
        frame(&mut state, 0, true, vec![enter]);
        assert_eq!(state.comments.iter().map(|c| c.body.as_str()).collect::<Vec<_>>(), ["draft"]);
        assert!(state.editor.is_none());
    }

    #[test]
    fn a_phone_wraps_a_long_line_under_its_indent() {
        // Ten two-point glyphs a row.
        let mono = |text: &str| wrapping(text, 20.0, |_| 2.0);
        let rows = |text: &str| mono(text).starts.len();
        assert_eq!(rows("short"), 0);
        assert_eq!(rows("0123456789"), 0, "a line that just fits");
        assert_eq!(rows("0123456789abcdefghij"), 1);
        // Four spaces in: the continuations hang under the code, six glyphs a row.
        let line = "    step(1); // xxxxxxxx";
        let wrapped = mono(line);
        assert_eq!(wrapped, Wrapped { indent: 8.0, starts: vec![20.0, 32.0, 44.0] });
        let piece = |n| Piece::of(&wrapped, n, 20.0);
        assert_eq!(piece(0), Piece { start: 0.0, indent: 0.0, width: 20.0 });
        assert_eq!(piece(1), Piece { start: 20.0, indent: 8.0, width: 12.0 });
        assert_eq!(piece(3), Piece { start: 44.0, indent: 8.0, width: 12.0 });
        // A tab is four spaces; an indent past half a row hangs at half.
        assert_eq!(mono("\tx").indent, 8.0);
        let deep = mono(&format!("{}x{}", " ".repeat(9), "y".repeat(10)));
        assert_eq!((deep.indent, deep.starts.len()), (10.0, 2));
    }

    #[test]
    fn a_phone_wraps_wide_glyphs_by_their_width() {
        // CJK glyphs twice as wide as Latin ones: five fit a row, and none is cut in two.
        let glyph = |c: char| if c.is_ascii() { 2.0 } else { 4.0 };
        let wrapped = wrapping("日本語のテキストです", 20.0, glyph);
        assert_eq!(wrapped.starts, [20.0]);
        assert_eq!(Piece::of(&wrapped, 1, 20.0), Piece { start: 20.0, indent: 0.0, width: 20.0 });
        // A glyph that would cross the row's edge starts the next row instead.
        let wrapped = wrapping("abcdefghi日x", 20.0, glyph);
        assert_eq!(wrapped.starts, [18.0]);
        assert_eq!(Piece::of(&wrapped, 0, 20.0).width, 18.0, "the first row ends before the glyph");
        let emoji = wrapping(&"🦀".repeat(7), 20.0, |c| if c == '🦀' { 6.0 } else { 2.0 });
        assert_eq!(emoji.starts, [18.0, 36.0], "three to a row");
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

    #[test]
    fn shift_arrows_skip_hunks_and_always_land_on_file_headers() {
        let mut state = state_with(&["a.rs", "b.rs"]);
        let prepared = Arc::get_mut(state.prepared.as_mut().unwrap()).unwrap();
        let hunk = prepared.diff.files[0].hunks[0].clone();
        prepared.diff.files[0].hunks.push(hunk);
        state.open("a.rs");
        state.hunk = Some(1);
        assert_eq!(state.step(1).unwrap().0, "b.rs");
        assert!(matches!(state.scroll_to, Some(ScrollTarget::File(1))));
        assert_eq!(state.step(-1).unwrap().0, "a.rs");
        assert!(matches!(state.scroll_to, Some(ScrollTarget::File(0))));
        assert!(state.hunk.is_none());
        let mut backwards = state_with(&["first", "last"]);
        assert_eq!(backwards.step(-1).unwrap().0, "last");
        let mut single = state_with(&["only"]);
        for delta in [1, -1, 1] {
            single.hunk = Some(0);
            assert_eq!(single.step(delta).unwrap().0, "only");
            assert!(matches!(single.scroll_to, Some(ScrollTarget::File(0))));
            assert!(single.hunk.is_none());
        }
    }

    #[test]
    fn page_keys_walk_hunks_across_files_in_both_directions() {
        let mut state = state_with(&["a.rs", "binary", "c.rs"]);
        let prepared = Arc::get_mut(state.prepared.as_mut().unwrap()).unwrap();
        let mut second = prepared.diff.files[0].hunks[0].clone();
        second.new_start = 10;
        for line in &mut second.lines {
            line.new_no = line.new_no.map(|n| n + 9);
        }
        prepared.diff.files[0].hunks.push(second);
        prepared.diff.files[1].hunks.clear();
        prepared.diff.files[1].binary = true;
        state.collapsed.insert("a.rs".into());
        for (delta, path, line) in [
            (1, "a.rs", Some(1)),
            (1, "a.rs", Some(10)),
            (1, "binary", None),
            (1, "c.rs", Some(1)),
            (1, "c.rs", Some(1)),
            (-1, "binary", None),
            (-1, "a.rs", Some(10)),
            (-1, "a.rs", Some(1)),
            (-1, "a.rs", Some(1)),
        ] {
            assert_eq!(state.step_hunk(delta), Some((path.into(), line)));
            assert_eq!(state.focused.as_deref(), Some(path));
            assert!(!state.collapsed.contains(path));
        }
        assert!(matches!(state.scroll_to, Some(ScrollTarget::Hunk(0, 0))));
        state.open("c.rs");
        assert_eq!(state.step_hunk(-1), Some(("binary".into(), None)));
        let mut backwards = state_with(&["first", "last"]);
        assert_eq!(backwards.step_hunk(-1).unwrap().0, "last");
        assert!(DiffState::default().step_hunk(1).is_none());
        assert!(state_with(&[]).step_hunk(-1).is_none());
    }
}
