//! The terminal painter: draws a [`Screen`], the cell model `atomics.v1` streams, with egui. The
//! desktop paints its own terminals through it as well, from a copy of the cells it displays, so
//! the browser and the desktop draw the same cells the same way.
//!
//! The look is the desktop's: one monospace font (bold and italic are not drawn), every underline
//! style as one thin line in the text colour, no strikeout, and the theme's cursor colour. ASCII
//! text is laid out in runs; any other character, with its combining characters, is centred over
//! its one or two columns. Combining characters over an ASCII letter are not drawn, as the desktop
//! never drew them.
//!
//! [`encode`] turns keys, text, a paste and mouse reports into what the terminal is sent.

pub mod encode;

use atomics_core::screen::{
    BACKGROUND, Color, CursorShape, FOREGROUND, LAST_SLOT, Line, PALETTE_LEN, Rgb, Screen, Style, attr,
};
use egui::{Align2, Color32, FontId, Painter, Pos2, Rect, Shape, Stroke, StrokeKind, Vec2, pos2, vec2};

/// The font and the size of one cell.
pub struct Metrics {
    pub font: FontId,
    pub cell: Vec2,
}

impl Metrics {
    /// `palette`'s monospace font at the theme's size, else at `size` (the config's), and its cell.
    pub fn new(ui: &egui::Ui, palette: &crate::theme::Palette, size: f32) -> Self {
        let font = palette.mono(palette.fonts.mono_size.unwrap_or(size));
        let cell = ui.fonts_mut(|f| vec2(f.glyph_width(&font, 'M'), f.row_height(&font)));
        Self { font, cell: vec2(cell.x, cell.y.ceil()) }
    }
}

/// The theme colours drawn around and over the cells. Cells take their own colours from the
/// screen's palette.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    /// Behind the whole terminal. Cells with this background get no fill of their own.
    pub background: Color32,
    pub selection: Color32,
    pub cursor: Color32,
}

/// A cell in the painted screen: `row` counts from its top line, negative above it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Point {
    pub row: i32,
    pub col: u16,
}

/// The cells from `start` to `end`, both included. A block selection takes the same columns on
/// every row; otherwise the rows between take whole lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub start: Point,
    pub end: Point,
    pub block: bool,
}

impl Selection {
    pub fn contains(&self, row: i32, col: u16) -> bool {
        let (start, end) = (self.start, self.end);
        start.row <= row
            && end.row >= row
            && (start.col <= col || (start.row != row && !self.block))
            && (end.col >= col || (end.row != row && !self.block))
    }
}

/// Terminal text faded toward the terminal background.
pub fn dim(background: Color32, color: Color32) -> Color32 {
    crate::theme::lerp(background, color, 0.62)
}

impl crate::theme::Palette {
    /// The theme colours the terminal painter draws around and over the cells.
    pub fn terminal_theme(&self) -> Theme {
        Theme { background: self.terminal, selection: self.selection, cursor: self.cursor }
    }
}

fn rgb(Rgb([r, g, b]): Rgb) -> Color32 {
    Color32::from_rgb(r, g, b)
}

/// Every palette slot's colour. Slots past the palette are derived from it, as
/// `screen::LAST_SLOT` describes; an unknown slot is the foreground.
struct Slots([Color32; LAST_SLOT as usize + 1]);

impl Slots {
    fn new(palette: &[Rgb], background: Color32) -> Self {
        let slot = |i: usize| rgb(palette.get(i).copied().unwrap_or_default());
        let foreground = slot(usize::from(FOREGROUND));
        Self(std::array::from_fn(|i| match i {
            i if i < PALETTE_LEN => slot(i),
            // Dim black to dim white.
            259..=266 => dim(background, slot(i - 259)),
            268 => dim(background, foreground),
            _ => foreground,
        }))
    }

    fn resolve(&self, color: Color) -> Color32 {
        match color {
            Color::Rgb(value) => rgb(value),
            Color::Slot(i) if i <= LAST_SLOT => self.0[usize::from(i)],
            Color::Slot(_) => self.0[usize::from(FOREGROUND)],
        }
    }
}

/// A screen's rows as they were last laid out: only a row whose cells changed is laid out again, and one that scrolled
/// is moved.
#[derive(Default)]
pub struct Kept {
    look: Option<Look>,
    first_line: u64,
    rows: Vec<Row>,
}

/// What the rows' shapes are made of besides their cells.
struct Look {
    /// A blank in the rows' font. The fonts hand out the same one until they are made anew, and the letters with them;
    /// it takes no room among them.
    blank: std::sync::Arc<egui::Galley>,
    cell: Vec2,
    theme: Theme,
    palette: Vec<Rgb>,
}

struct Row {
    /// Nothing while some of it was selected.
    line: Option<Line>,
    /// With the row's first cell at the origin.
    shapes: Vec<Shape>,
}

impl Kept {
    /// Lets go of what was laid out with another look.
    fn look(&mut self, painter: &Painter, screen: &Screen, metrics: &Metrics, theme: &Theme) {
        let blank = painter.layout_no_wrap(" ".into(), metrics.font.clone(), theme.cursor);
        if self.look.as_ref().is_some_and(|look| {
            std::sync::Arc::ptr_eq(&look.blank, &blank)
                && look.cell == metrics.cell
                && look.theme == *theme
                && look.palette == screen.palette
        }) {
            return;
        }
        self.rows.clear();
        self.look = Some(Look { blank, cell: metrics.cell, theme: *theme, palette: screen.palette.clone() });
    }

    /// The rows of `screen`, laid out where they are not already. `same` when its cells are those of the last call.
    fn lay_out(
        &mut self,
        painter: &Painter,
        screen: &Screen,
        selection: Option<&Selection>,
        metrics: &Metrics,
        theme: &Theme,
        same: bool,
    ) {
        let scrolled = screen.first_line.wrapping_sub(self.first_line) as i64;
        self.first_line = screen.first_line;
        let mut old: Vec<Option<Row>> = std::mem::take(&mut self.rows).into_iter().map(Some).collect();
        let mut slots = None;
        self.rows.reserve(screen.lines.len());
        for (index, line) in screen.lines.iter().enumerate() {
            let selection = selection.filter(|s| (s.start.row..=s.end.row).contains(&(index as i32)));
            let was = usize::try_from((index as i64).saturating_add(scrolled)).unwrap_or(index);
            let kept = [was, index].into_iter().find_map(|at| {
                old.get_mut(at)?.take_if(|row| {
                    selection.is_none() && row.line.as_ref().is_some_and(|old| same && at == index || old == line)
                })
            });
            self.rows.push(kept.unwrap_or_else(|| {
                let slots = slots.get_or_insert_with(|| Slots::new(&screen.palette, theme.background));
                Row {
                    shapes: row_shapes(painter, (index as i32, line), selection, slots, metrics, theme),
                    line: selection.is_none().then(|| line.clone()),
                }
            }));
        }
    }
}

/// The shapes of a row of cells, with its first cell at the origin.
fn row_shapes(
    painter: &Painter,
    (row, line): (i32, &Line),
    selection: Option<&Selection>,
    slots: &Slots,
    metrics: &Metrics,
    theme: &Theme,
) -> Vec<Shape> {
    // Default blanks draw nothing of their own while the default background is the theme's.
    let plain_blanks = slots.resolve(Color::Slot(BACKGROUND)) == theme.background;
    let cell = metrics.cell;
    let at = |col: usize| pos2(col as f32 * cell.x, 0.0);
    let mut shapes = Vec::new();
    let mut run = Run::default();
    for (col, c) in line.cells.iter().enumerate() {
        // The column a wide character covers is drawn with it.
        if c.width == 0 {
            continue;
        }
        let ch = c.c;
        let blank = ch == ' ' || ch == '\0';
        let selected = selection.is_some_and(|s| s.contains(row, col as u16));
        if blank && plain_blanks && !selected && is_plain(&c.style) {
            run.blank(col);
            continue;
        }
        let style = &c.style;
        let mut fg = slots.resolve(style.fg);
        let mut bg = slots.resolve(style.bg);
        if style.attrs & attr::INVERSE != 0 {
            std::mem::swap(&mut fg, &mut bg);
        }
        if style.attrs & attr::DIM != 0 {
            fg = dim(theme.background, fg);
        }
        if style.attrs & attr::HIDDEN != 0 {
            fg = bg;
        }
        if selected {
            bg = theme.selection;
        }
        let width = if c.width == 2 { 2.0 } else { 1.0 };
        if bg != theme.background {
            shapes.push(Shape::rect_filled(Rect::from_min_size(at(col), vec2(cell.x * width, cell.y)), 0.0, bg));
        }
        if style.attrs & attr::ALL_UNDERLINES != 0 {
            let (x, y) = (at(col).x, cell.y - 1.5);
            shapes.push(Shape::line_segment([pos2(x, y), pos2(x + cell.x * width, y)], Stroke::new(1.0, fg)));
        }

        if blank {
            run.blank(col);
            continue;
        }
        // Runs carry the base character only, as the desktop always drew them.
        let simple = ch.is_ascii() && width == 1.0;
        if !run.continues(col, fg) || !simple {
            run.flush(painter, &at, metrics, &mut shapes);
        }
        if simple {
            if run.text.is_empty() {
                run = Run { col, fg, text: std::mem::take(&mut run.text), next_col: col };
            }
            run.text.push(ch);
            run.next_col = col + 1;
        } else {
            let mut text = String::from(ch);
            if let Some(extra) = &c.combining {
                text.extend(extra.iter());
            }
            let center = at(col) + vec2(cell.x * width / 2.0, cell.y / 2.0);
            let galley = painter.layout_no_wrap(text, metrics.font.clone(), fg);
            let rect = Align2::CENTER_CENTER.anchor_size(center, galley.size());
            shapes.push(Shape::galley(rect.min, galley, fg));
        }
    }
    run.flush(painter, &at, metrics, &mut shapes);
    shapes
}

fn moved(shape: &Shape, delta: Vec2) -> Shape {
    let mut shape = shape.clone();
    match &mut shape {
        Shape::Text(text) => text.pos += delta,
        Shape::Rect(rect) => rect.rect = rect.rect.translate(delta),
        Shape::LineSegment { points, .. } => points.iter_mut().for_each(|point| *point += delta),
        shape => shape.translate(delta),
    }
    shape
}

/// Paints `screen` with its top-left cell at `inner.min` over a `theme.background` fill of `outer`,
/// clipped to `outer`. A focused terminal shows its cursor's shape; an unfocused one an outline.
/// `kept` is this terminal's from the last time, and `same` says its cells have not changed since.
#[allow(clippy::too_many_arguments)]
pub fn paint(
    painter: &Painter,
    (outer, inner): (Rect, Rect),
    screen: &Screen,
    selection: Option<&Selection>,
    metrics: &Metrics,
    theme: &Theme,
    focused: bool,
    (kept, same): (&mut Kept, bool),
) {
    let painter = painter.with_clip_rect(outer);
    painter.rect_filled(outer, 0.0, theme.background);

    let cell = metrics.cell;
    let origin = inner.min;
    let at = |row: i32, col: usize| pos2(origin.x + col as f32 * cell.x, origin.y + row as f32 * cell.y);
    kept.look(&painter, screen, metrics, theme);
    kept.lay_out(&painter, screen, selection, metrics, theme, same);
    for (index, row) in kept.rows.iter().enumerate() {
        let delta = at(index as i32, 0).to_vec2();
        // A row under or over what shows of the terminal is not drawn.
        let top = origin.y + index as f32 * cell.y;
        if top > painter.clip_rect().bottom() || top + cell.y < painter.clip_rect().top() {
            continue;
        }
        painter.extend(row.shapes.iter().map(|shape| moved(shape, delta)));
    }

    let cursor = screen.cursor;
    if cursor.shape != CursorShape::Hidden && cursor.row < screen.rows {
        let r = Rect::from_min_size(at(i32::from(cursor.row), usize::from(cursor.col)), cell);
        match (focused, cursor.shape) {
            (true, CursorShape::Beam) => {
                painter.rect_filled(r.with_max_x(r.left() + 2.0), 0.0, theme.cursor);
            }
            (true, CursorShape::Underline) => {
                painter.rect_filled(r.with_min_y(r.bottom() - 2.0), 0.0, theme.cursor);
            }
            (true, _) => {
                painter.rect_filled(r, 0.0, theme.cursor.gamma_multiply(0.7));
            }
            (false, _) => {
                painter.rect_stroke(r, 0.0, Stroke::new(1.0, theme.cursor.gamma_multiply(0.6)), StrokeKind::Inside);
            }
        }
    }
}

/// The default style, matched field by field: cheaper than comparing in unoptimised builds.
fn is_plain(style: &Style) -> bool {
    matches!(style, Style { fg: Color::Slot(FOREGROUND), bg: Color::Slot(BACKGROUND), attrs: 0, underline: None })
}

/// Consecutive ASCII cells of a row with the same color, laid out as one text shape.
#[derive(Default)]
struct Run {
    col: usize,
    next_col: usize,
    fg: Color32,
    text: String,
}

impl Run {
    /// Blanks extend a run whatever their color, so a line stays one shape.
    fn blank(&mut self, col: usize) {
        if !self.text.is_empty() && self.next_col == col {
            self.text.push(' ');
            self.next_col += 1;
        }
    }

    fn continues(&self, col: usize, fg: Color32) -> bool {
        self.text.is_empty() || (self.next_col == col && self.fg == fg)
    }

    fn flush(&mut self, painter: &Painter, at: &impl Fn(usize) -> Pos2, metrics: &Metrics, shapes: &mut Vec<Shape>) {
        if self.text.is_empty() {
            return;
        }
        let mut text = std::mem::take(&mut self.text);
        text.truncate(text.trim_end().len());
        let galley = painter.layout_no_wrap(text, metrics.font.clone(), self.fg);
        shapes.push(Shape::galley(at(self.col), galley, self.fg));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{Fonts, Palette};
    use atomics_core::config::ThemeConfig;
    use atomics_core::screen::{Cell, Cursor};

    /// The default built-in theme, as the desktop hands it to the painter.
    fn palette() -> Palette {
        Palette::new(&atomics_core::theme::resolve(&ThemeConfig::default(), |_| None).tokens, Fonts::default())
    }

    fn theme() -> Theme {
        palette().terminal_theme()
    }

    fn shapes(screen: &Screen, selection: Option<&Selection>, focused: bool) -> Vec<egui::Shape> {
        let ctx = egui::Context::default();
        let metrics = Metrics { font: FontId::monospace(12.0), cell: vec2(8.0, 16.0) };
        let outer = Rect::from_min_size(Pos2::ZERO, vec2(200.0, 100.0));
        let mut output = ctx.run_ui(egui::RawInput { screen_rect: Some(outer), ..Default::default() }, |ui| {
            let inner = outer.shrink2(vec2(6.0, 4.0));
            let kept = (&mut Kept::default(), false);
            paint(ui.painter(), (outer, inner), screen, selection, &metrics, &theme(), focused, kept);
        });
        output.textures_delta.clear();
        output.shapes.into_iter().map(|clipped| clipped.shape).collect()
    }

    fn texts(shapes: &[egui::Shape]) -> Vec<String> {
        shapes
            .iter()
            .filter_map(|shape| match shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                _ => None,
            })
            .collect()
    }

    fn screen(rows: &[&str]) -> Screen {
        let cols = 20;
        let mut screen = Screen::blank(cols, rows.len() as u16);
        for (line, text) in screen.lines.iter_mut().zip(rows) {
            for (cell, c) in line.cells.iter_mut().zip(text.chars()) {
                cell.c = c;
            }
        }
        // The theme's colours in every slot, as the desktop's copy fills them.
        let p = palette();
        for (slot, rgb) in screen.palette.iter_mut().enumerate() {
            let color = match slot {
                256 => p.foreground,
                257 => p.terminal,
                258 => p.cursor,
                i => p.indexed(i),
            };
            *rgb = Rgb([color.r(), color.g(), color.b()]);
        }
        screen.cursor.shape = CursorShape::Hidden;
        screen
    }

    /// What one frame after another paints of these screens, with what it kept from the one before.
    fn painted(ctx: &egui::Context, kept: &mut Kept, screen: &Screen, size: f32, same: bool) -> Vec<egui::Shape> {
        let metrics = Metrics { font: FontId::monospace(size), cell: vec2(8.0, 16.0) };
        let outer = Rect::from_min_size(Pos2::ZERO, vec2(200.0, 100.0));
        let mut output = ctx.run_ui(egui::RawInput { screen_rect: Some(outer), ..Default::default() }, |ui| {
            let rects = (outer, outer.shrink2(vec2(6.0, 4.0)));
            paint(ui.painter(), rects, screen, None, &metrics, &theme(), true, (kept, same));
        });
        output.textures_delta.clear();
        output.shapes.into_iter().map(|clipped| clipped.shape).collect()
    }

    fn galleys(shapes: &[egui::Shape]) -> Vec<std::sync::Arc<egui::Galley>> {
        let galley = |shape: &egui::Shape| match shape {
            egui::Shape::Text(text) => Some(text.galley.clone()),
            _ => None,
        };
        shapes.iter().filter_map(galley).collect()
    }

    #[test]
    fn only_a_changed_row_is_laid_out_again_and_a_scrolled_one_moves() {
        let ctx = egui::Context::default();
        let mut kept = Kept::default();
        let mut s = screen(&["one", "two", "three"]);
        let first = galleys(&painted(&ctx, &mut kept, &s, 12.0, false));
        s.lines[1].cells[0].c = 'T';
        let shapes = painted(&ctx, &mut kept, &s, 12.0, false);
        assert_eq!(texts(&shapes), ["one", "Two", "three"]);
        let second = galleys(&shapes);
        let kept_rows = |old: &[std::sync::Arc<egui::Galley>], new: &[std::sync::Arc<egui::Galley>]| {
            new.iter().map(|new| old.iter().position(|old| std::sync::Arc::ptr_eq(old, new))).collect::<Vec<_>>()
        };
        assert_eq!(kept_rows(&first, &second), [Some(0), None, Some(2)]);

        // A line is printed: the rows move up, and the last one is new.
        s.lines.remove(0);
        s.lines.push(screen(&["four"]).lines.remove(0));
        s.first_line += 1;
        let shapes = painted(&ctx, &mut kept, &s, 12.0, false);
        assert_eq!(texts(&shapes), ["Two", "three", "four"]);
        assert_eq!(kept_rows(&second, &galleys(&shapes)), [Some(1), Some(2), None]);
        let fresh = painted(&ctx, &mut Kept::default(), &s, 12.0, false);
        let places = |shapes: &[egui::Shape]| shapes.iter().map(|s| s.visual_bounding_rect()).collect::<Vec<_>>();
        assert_eq!(places(&shapes), places(&fresh), "moved to where it would have been laid out");

        // Scrolled back to lines above, and to fewer rows.
        s.first_line -= 1;
        s.lines.insert(0, screen(&["one"]).lines.remove(0));
        s.lines.truncate(2);
        s.rows = 2;
        assert_eq!(texts(&painted(&ctx, &mut kept, &s, 12.0, false)), ["one", "Two"]);
        assert_eq!(texts(&painted(&ctx, &mut kept, &s, 12.0, true)), ["one", "Two"]);
    }

    #[test]
    fn another_font_or_palette_lays_every_row_out_again() {
        let ctx = egui::Context::default();
        let mut kept = Kept::default();
        let mut s = screen(&["one", "two"]);
        let small = galleys(&painted(&ctx, &mut kept, &s, 12.0, false));
        let large = galleys(&painted(&ctx, &mut kept, &s, 20.0, true));
        assert!(small.iter().zip(&large).all(|(small, large)| large.size().y > small.size().y));
        s.palette[usize::from(FOREGROUND)] = Rgb([1, 2, 3]);
        let shapes = painted(&ctx, &mut kept, &s, 20.0, true);
        assert!(shapes.iter().all(|shape| match shape {
            egui::Shape::Text(text) => text.fallback_color.to_array() == [1, 2, 3, 255],
            _ => true,
        }));
        // The fonts are made anew, and their letters elsewhere in the texture.
        let before = galleys(&shapes);
        ctx.set_fonts(egui::FontDefinitions::empty());
        ctx.set_fonts(egui::FontDefinitions::default());
        let mut after = Vec::new();
        for _ in 0..2 {
            after = galleys(&painted(&ctx, &mut kept, &s, 20.0, true));
        }
        assert!(before.iter().zip(&after).all(|(old, new)| !std::sync::Arc::ptr_eq(old, new)));
    }

    #[test]
    fn a_selection_shows_and_goes_on_rows_that_did_not_change() {
        let ctx = egui::Context::default();
        let (mut kept, s) = (Kept::default(), screen(&["one", "two"]));
        let metrics = Metrics { font: FontId::monospace(12.0), cell: vec2(8.0, 16.0) };
        let outer = Rect::from_min_size(Pos2::ZERO, vec2(200.0, 100.0));
        let selection = Selection { start: Point { row: 1, col: 0 }, end: Point { row: 1, col: 2 }, block: false };
        let mut fills = |selection: Option<&Selection>| {
            let mut output = ctx.run_ui(egui::RawInput { screen_rect: Some(outer), ..Default::default() }, |ui| {
                paint(ui.painter(), (outer, outer), &s, selection, &metrics, &theme(), true, (&mut kept, true));
            });
            output.textures_delta.clear();
            let filled = |clipped: &egui::epaint::ClippedShape| matches!(&clipped.shape, egui::Shape::Rect(rect) if rect.fill == theme().selection);
            output.shapes.iter().filter(|shape| filled(shape)).count()
        };
        assert_eq!([fills(None), fills(Some(&selection)), fills(None)], [0, 3, 0]);
    }

    #[test]
    fn ascii_runs_join_across_blanks_and_split_on_colour_and_wide_cells() {
        let mut s = screen(&["ab cd  ", "xy"]);
        s.lines[1].cells[2] = Cell { c: '中', combining: None, width: 2, style: Style::default() };
        s.lines[1].cells[3].width = 0;
        s.lines[1].cells[4].c = 'z';
        s.lines[1].cells[5] =
            Cell { c: 'r', style: Style { fg: Color::Slot(1), ..Style::default() }, ..Cell::default() };
        assert_eq!(texts(&shapes(&s, None, true)), ["ab cd", "xy", "中", "z", "r"]);
    }

    #[test]
    fn combining_characters_draw_over_their_base_only_outside_ascii_runs() {
        let mut s = screen(&["e", ""]);
        s.lines[0].cells[0].combining = Some(vec!['\u{301}'].into_boxed_slice());
        s.lines[1].cells[0] = Cell { c: 'а', combining: Some(vec!['\u{301}'].into_boxed_slice()), ..Cell::default() };
        assert_eq!(texts(&shapes(&s, None, true)), ["e", "а\u{301}"]);
    }

    #[test]
    fn colours_come_from_the_palette_with_inverse_dim_hidden_and_selection() {
        let mut s = screen(&["abcd"]);
        let red = Style { fg: Color::Slot(1), ..Style::default() };
        s.lines[0].cells[0].style = Style { attrs: attr::INVERSE, ..red };
        s.lines[0].cells[1].style = Style { attrs: attr::DIM, ..red };
        s.lines[0].cells[2].style = Style { attrs: attr::HIDDEN | attr::UNDERCURL, ..red };
        s.lines[0].cells[3].style = Style { fg: Color::Rgb(Rgb([1, 2, 3])), ..Style::default() };
        let selection = Selection { start: Point { row: 0, col: 3 }, end: Point { row: 0, col: 3 }, block: false };
        let shapes = shapes(&s, Some(&selection), true);
        let fills: Vec<Color32> = shapes
            .iter()
            .filter_map(|shape| match shape {
                egui::Shape::Rect(rect) => Some(rect.fill),
                _ => None,
            })
            .collect();
        let (theme, p) = (theme(), palette());
        let red = p.ansi[1];
        assert_eq!(fills, [theme.background, red, theme.selection], "the fill, inverse red, the selection");
        let underline = shapes.iter().find_map(|shape| match shape {
            egui::Shape::LineSegment { stroke, .. } => Some(stroke.color),
            _ => None,
        });
        assert_eq!(underline, Some(theme.background), "hidden text is drawn in its background");
        let colors: Vec<Color32> = shapes
            .iter()
            .filter_map(|shape| match shape {
                egui::Shape::Text(text) => Some(text.fallback_color),
                _ => None,
            })
            .collect();
        assert_eq!(colors.len(), 4);
        assert_eq!(colors[..3], [p.terminal, dim(theme.background, red), p.terminal], "inverse, dim and hidden");
        assert_eq!(colors[3].to_array(), [1, 2, 3, 255], "a truecolor cell keeps its colour");
    }

    #[test]
    fn selection_follows_lines_or_blocks() {
        let line = Selection { start: Point { row: -2, col: 5 }, end: Point { row: 1, col: 2 }, block: false };
        assert!(line.contains(-2, 5) && !line.contains(-2, 4) && line.contains(-1, 0) && line.contains(0, 99));
        assert!(line.contains(1, 2) && !line.contains(1, 3) && !line.contains(2, 0));
        let block = Selection { block: true, start: Point { row: -2, col: 1 }, ..line };
        assert!(block.contains(0, 1) && block.contains(0, 2) && !block.contains(0, 3) && !block.contains(0, 0));
    }

    #[test]
    fn derived_slots_dim_the_palette() {
        let mut palette = vec![Rgb::default(); PALETTE_LEN];
        palette[3] = Rgb([100, 200, 50]);
        palette[usize::from(FOREGROUND)] = Rgb([220, 220, 220]);
        let bg = theme().background;
        let slots = Slots::new(&palette, bg);
        let (yellow, fg) = (slots.resolve(Color::Slot(3)), slots.resolve(Color::Slot(FOREGROUND)));
        assert_eq!([yellow.to_array(), fg.to_array()], [[100, 200, 50, 255], [220, 220, 220, 255]], "the palette's");
        assert_eq!(slots.resolve(Color::Slot(262)), dim(bg, yellow), "dim yellow");
        assert_eq!(slots.resolve(Color::Slot(267)), fg, "bright foreground");
        assert_eq!(slots.resolve(Color::Slot(268)), dim(bg, fg), "dim foreground");
        assert_eq!(slots.resolve(Color::Slot(999)), fg, "unknown slots");
    }

    #[test]
    fn the_cursor_is_solid_when_focused_and_an_outline_otherwise() {
        let mut s = screen(&["$ "]);
        let theme = theme();
        for (shape, focused, fill) in [
            (CursorShape::Block, true, Some(theme.cursor.gamma_multiply(0.7))),
            (CursorShape::HollowBlock, true, Some(theme.cursor.gamma_multiply(0.7))),
            (CursorShape::Beam, true, Some(theme.cursor)),
            (CursorShape::Underline, true, Some(theme.cursor)),
            (CursorShape::Beam, false, None),
            (CursorShape::Hidden, true, None),
        ] {
            s.cursor = Cursor { row: 0, col: 2, shape, blinking: false };
            let shapes = shapes(&s, None, focused);
            let last = match shapes.last() {
                Some(egui::Shape::Rect(rect)) => Some(rect),
                _ => None,
            };
            match (shape, focused) {
                (CursorShape::Hidden, _) => assert!(last.is_none_or(|r| r.fill == theme.background)),
                (_, false) => assert_eq!(last.unwrap().stroke.color, theme.cursor.gamma_multiply(0.6)),
                _ => assert_eq!(Some(last.unwrap().fill), fill, "{shape:?}"),
            }
        }
    }
}
