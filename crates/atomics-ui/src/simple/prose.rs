//! The chat's Markdown. Running text is laid out a paragraph at a time, so code, bold and links wrap with the
//! words around them and share one baseline. Code spans sit on a rounded chip in the monospace face; list markers
//! sit on the first line's x-height; code blocks carry their language and a Copy button. Text stays selectable.

use std::ops::Range;
use std::sync::Arc;

use egui::text::{LayoutJob, LayoutSection, TextFormat};
use egui::{Align2, Color32, Galley, Pos2, Rect, Sense, Stroke, vec2};

use super::look::{self, Ink, Weight};
use crate::flow::font;
use crate::markdown::{self, Block, Inline};
use crate::theme::{Palette, lerp};

/// Room around a code span's chip, each side, in points.
const CHIP_PAD: f32 = 4.0;
/// How far a list's items step in per level, and the width their markers take.
const INDENT: f32 = 20.0;
const MARKER: f32 = 22.0;

/// What a paragraph's laid-out text holds besides plain words: where its code spans and links are, by char.
#[derive(Default)]
struct Spans {
    code: Vec<Range<usize>>,
    links: Vec<(Range<usize>, String)>,
}

/// The kind of block before this one, for the space above it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Heading,
    Item,
    Code,
}

/// `source` as the chat shows the chief's words, at the body size.
pub fn show(ui: &mut egui::Ui, source: &str, p: &Palette) {
    let ink = Ink::of(p);
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        let mut previous: Option<Kind> = None;
        for block in markdown::parse(source) {
            let kind = match &block {
                Block::Heading(..) => Kind::Heading,
                Block::Item(..) => Kind::Item,
                Block::Code { .. } | Block::Table { .. } | Block::Rule => Kind::Code,
                Block::Paragraph(_) | Block::Quote(_) => Kind::Text,
            };
            if let Some(previous) = previous {
                ui.add_space(gap(previous, kind));
            }
            previous = Some(kind);
            match block {
                Block::Heading(level, spans) => {
                    let size = match level {
                        1 => 20.0,
                        2 => 17.0,
                        _ => look::BODY,
                    };
                    paragraph(ui, &spans, size, Weight::Semibold, ink.primary, p);
                }
                Block::Paragraph(spans) => {
                    paragraph(ui, &spans, look::BODY, Weight::Regular, ink.primary, p);
                }
                Block::Quote(spans) => quote(ui, &spans, &ink, p),
                Block::Item(depth, marker, spans) => item(ui, depth, &marker, &spans, &ink, p),
                Block::Code { language, text } => code_block(ui, &language, &text, &ink, p),
                Block::Table { header, align, rows } => {
                    crate::tickets::markdown_table(ui, &header, &align, &rows, |ui, spans, heading| {
                        let weight = if heading { Weight::Semibold } else { Weight::Regular };
                        paragraph(ui, spans, look::LABEL + 1.0, weight, ink.primary, p);
                    });
                }
                Block::Rule => {
                    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 9.0), Sense::hover());
                    ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, p.separator));
                }
            }
        }
    });
}

/// The space above a block, after a block of `previous`'s kind.
fn gap(previous: Kind, next: Kind) -> f32 {
    match (previous, next) {
        (_, Kind::Heading) => 18.0,
        (Kind::Heading, _) => 6.0,
        (Kind::Item, Kind::Item) => 6.0,
        (Kind::Text, Kind::Item) => 8.0,
        _ => 12.0,
    }
}

/// One run of text, as the chat lays it out: the face at `size` and `weight`, code in the monospace face.
fn job(spans: &[Inline], size: f32, weight: Weight, color: Color32, width: f32, p: &Palette) -> (LayoutJob, Spans) {
    let line = look::leading(size);
    let format = |weight: Weight| {
        let mut format = TextFormat { font_id: font(size), color, line_height: Some(line), ..TextFormat::default() };
        if let Some(wght) = weight.axis() {
            format.coords.push("wght", wght);
        }
        format
    };
    let mut job = LayoutJob::default();
    job.wrap.max_width = width;
    let mut spans_at = Spans::default();
    let mut chars = 0;
    let mut pad_next = false;
    let mut push = |job: &mut LayoutJob, text: &str, format: TextFormat, lead: f32| {
        let start = job.text.len();
        job.text.push_str(text);
        job.sections.push(LayoutSection {
            leading_space: lead,
            byte_range: start.into()..job.text.len().into(),
            format,
        });
        let range = chars..chars + text.chars().count();
        chars = range.end;
        range
    };
    for span in spans {
        let lead = if std::mem::take(&mut pad_next) { CHIP_PAD } else { 0.0 };
        match span {
            Inline::Text(text) => {
                push(&mut job, text, format(weight), lead);
            }
            Inline::Bold(text) => {
                push(&mut job, text, format(Weight::Semibold), lead);
            }
            Inline::Italic(text) => {
                push(&mut job, text, TextFormat { italics: true, ..format(weight) }, lead);
            }
            Inline::Code(text) => {
                let code = TextFormat {
                    font_id: crate::theme::simple_mono(size * look::CODE / look::BODY),
                    ..format(Weight::Regular)
                };
                let range = push(&mut job, text, code, lead + CHIP_PAD);
                spans_at.code.push(range);
                pad_next = true;
            }
            Inline::Link { text, url } => {
                let range = push(&mut job, text, TextFormat { color: p.accent, ..format(weight) }, lead);
                spans_at.links.push((range, url.clone()));
            }
        }
    }
    if job.text.is_empty() {
        push(&mut job, " ", format(weight), 0.0);
    }
    (job, spans_at)
}

/// The rectangles a run of chars covers, one per row it is on, from the left of its first glyph to the right of its
/// last, between the top and bottom of its own face.
fn covered(galley: &Galley, chars: &Range<usize>) -> Vec<Rect> {
    let mut rects = Vec::new();
    let mut at = 0;
    for row in &galley.rows {
        let count = row.glyphs.len();
        let (from, to) = (chars.start.max(at), chars.end.min(at + count));
        if from < to {
            let (first, last) = (&row.glyphs[from - at], &row.glyphs[to - at - 1]);
            let top = row.pos.y + first.pos.y - first.font_ascent;
            rects.push(Rect::from_min_max(
                Pos2::new(row.pos.x + first.pos.x, top),
                Pos2::new(row.pos.x + last.max_x(), top + first.font_height),
            ));
        }
        at += count + usize::from(row.ends_with_newline);
    }
    rects
}

/// A run of text, selectable, with its code chips under it and its links live. Returns where its galley went.
fn paragraph(
    ui: &mut egui::Ui,
    spans: &[Inline],
    size: f32,
    weight: Weight,
    color: Color32,
    p: &Palette,
) -> (Pos2, Arc<Galley>) {
    let (job, at) = job(spans, size, weight, color, ui.available_width(), p);
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    let (pos, galley, response) = egui::Label::new(galley).selectable(true).layout_in_ui(ui);
    let painter = ui.painter();
    let chip = lerp(p.background, p.foreground, 0.085);
    for range in &at.code {
        for rect in covered(&galley, range) {
            let rect = rect.translate(pos.to_vec2()).expand2(vec2(CHIP_PAD, 1.0));
            painter.rect_filled(rect, 4.0, chip);
        }
    }
    let pointer = response.hover_pos();
    for (range, url) in &at.links {
        let rects: Vec<Rect> = covered(&galley, range).into_iter().map(|r| r.translate(pos.to_vec2())).collect();
        let hovered = pointer.is_some_and(|at| rects.iter().any(|r| r.contains(at)));
        if hovered && markdown::web_url(url).is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            for rect in &rects {
                painter.hline(rect.x_range(), rect.bottom() - 1.0, Stroke::new(1.0, p.accent));
            }
            response.clone().on_hover_text(markdown::web_url_hover(url));
            if response.clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(url));
            }
        }
    }
    egui::text_selection::LabelSelectionState::label_text_selection(
        ui,
        &response,
        pos,
        galley.clone(),
        color,
        Stroke::NONE,
    );
    (pos, galley)
}

/// A list item: its marker in the secondary ink, a bullet on the first line's x-height or a number on its baseline.
fn item(ui: &mut egui::Ui, depth: usize, marker: &str, spans: &[Inline], ink: &Ink, p: &Palette) {
    let indent = depth as f32 * INDENT;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        ui.add_space(indent + MARKER);
        let (pos, galley) = ui.vertical(|ui| paragraph(ui, spans, look::BODY, Weight::Regular, ink.primary, p)).inner;
        let Some(row) = galley.rows.first() else { return };
        let Some(glyph) = row.glyphs.first() else { return };
        let baseline = pos.y + row.pos.y + glyph.pos.y;
        let column = pos.x - MARKER;
        if marker == "•" {
            let middle = baseline - look::BODY * 0.27;
            let c = egui::pos2(column + 8.0, middle);
            if depth == 0 {
                ui.painter().circle_filled(c, 2.5, ink.secondary);
            } else {
                ui.painter().circle_stroke(c, 2.2, Stroke::new(1.2, ink.secondary));
            }
        } else {
            // Right-aligned in the marker's column, its baseline on the item's first line.
            let number = ui.painter().layout_no_wrap(marker.to_owned(), font(look::BODY), ink.secondary);
            let depth = number.rows.first().and_then(|r| r.glyphs.first()).map_or(0.0, |g| g.pos.y);
            let at = egui::pos2(column + MARKER - 7.0 - number.size().x, baseline - depth);
            ui.painter().galley(at, number, ink.secondary);
        }
    });
}

/// A quoted line: a bar in the theme's border colour, the words in the secondary ink.
fn quote(ui: &mut egui::Ui, spans: &[Inline], ink: &Ink, p: &Palette) {
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        ui.add_space(14.0);
        let (pos, galley) = ui.vertical(|ui| paragraph(ui, spans, look::BODY, Weight::Regular, ink.secondary, p)).inner;
        let bar = Rect::from_min_size(egui::pos2(pos.x - 14.0, pos.y + 2.0), vec2(3.0, galley.size().y - 4.0));
        ui.painter().rect_filled(bar, 1.5, lerp(p.border, p.foreground, 0.15));
    });
}

/// A fenced code block: a quiet card with its language and Copy above the code, which scrolls sideways rather than
/// wrap.
fn code_block(ui: &mut egui::Ui, language: &str, code: &str, ink: &Ink, p: &Palette) {
    let fill = lerp(p.background, p.foreground, 0.035);
    let edge = lerp(p.background, p.foreground, 0.1);
    egui::Frame::new().fill(fill).stroke(Stroke::new(1.0, edge)).corner_radius(10).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::hover());
        let label = if language.is_empty() { "code" } else { language };
        ui.painter().text(
            egui::pos2(bar.left() + 14.0, bar.center().y),
            Align2::LEFT_CENTER,
            label,
            font(look::SMALL),
            ink.secondary,
        );
        let id = ui.scope_id().with(("code-copy", code));
        let now = ui.input(|i| i.time);
        let copied = ui.data(|d| d.get_temp::<f64>(id)).is_some_and(|at| now - at < 1.6);
        let word = if copied { "Copied" } else { "Copy" };
        let size = ui.painter().layout_no_wrap(word.into(), font(look::SMALL), ink.secondary).size();
        let button = Rect::from_center_size(
            egui::pos2(bar.right() - 14.0 - size.x / 2.0, bar.center().y),
            vec2(size.x + 14.0, 22.0),
        );
        let response = ui.interact(button, id, Sense::click()).on_hover_text("Copy the code");
        if response.hovered() {
            ui.painter().rect_filled(button, 6.0, p.hover());
        }
        let color = if response.hovered() || copied { ink.primary } else { ink.secondary };
        ui.painter().text(button.center(), Align2::CENTER_CENTER, word, font(look::SMALL), color);
        if response.clicked() {
            ui.ctx().copy_text(code.to_owned());
            ui.data_mut(|d| d.insert_temp(id, now));
        }
        if copied {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(300));
        }
        ui.painter().hline(bar.x_range(), bar.bottom(), Stroke::new(1.0, edge));
        egui::ScrollArea::horizontal().id_salt(id.with("scroll")).auto_shrink([false, true]).show(ui, |ui| {
            egui::Frame::new().inner_margin(egui::Margin::symmetric(14, 11)).show(ui, |ui| {
                let format = TextFormat {
                    font_id: crate::theme::simple_mono(look::CODE_BLOCK),
                    color: ink.primary,
                    line_height: Some((look::CODE_BLOCK * 1.5).round()),
                    ..TextFormat::default()
                };
                let job = LayoutJob::single_section(code.trim_end_matches('\n').to_owned(), format);
                ui.add(egui::Label::new(job).wrap_mode(egui::TextWrapMode::Extend).selectable(true));
            });
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn galley(ctx: &egui::Context, spans: &[Inline], width: f32) -> (Arc<Galley>, Spans) {
        let p = crate::Client::default().palette;
        let mut out = None;
        let mut frame = ctx.run_ui(Default::default(), |ui| {
            let (job, spans) = job(spans, look::BODY, Weight::Regular, p.foreground, width, &p);
            out = Some((ui.fonts_mut(|f| f.layout_job(job)), spans));
        });
        frame.textures_delta.clear();
        out.unwrap()
    }

    #[test]
    fn a_code_span_inside_bold_is_code_on_one_baseline_with_its_words() {
        let ctx = egui::Context::default();
        let spans = markdown::inlines("Open **`recipes.py`** to add it.");
        let (galley, at) = galley(&ctx, &spans, 600.0);
        assert_eq!(galley.text(), "Open recipes.py to add it.", "no backticks left in the text");
        assert_eq!(at.code.len(), 1);
        assert_eq!(at.code[0], 5..15);
        assert_eq!(galley.rows.len(), 1, "one row: the chip does not break the line");
        let rects = covered(&galley, &at.code[0]);
        assert_eq!(rects.len(), 1);
        let word = covered(&galley, &(0..4))[0];
        assert!(rects[0].left() > word.right(), "the chip starts after the word before it");
    }

    #[test]
    fn a_code_span_that_wraps_gets_a_chip_on_each_of_its_rows() {
        let ctx = egui::Context::default();
        let spans = markdown::inlines("Run `python3 recipes.py show pancakes --verbose` and read what it prints.");
        let (galley, at) = galley(&ctx, &spans, 190.0);
        let rects = covered(&galley, &at.code[0]);
        assert!(rects.len() >= 2, "a chip per row: {rects:?}");
        assert!(rects.windows(2).all(|pair| pair[1].top() > pair[0].top()), "each on its own row: {rects:?}");
    }

    #[test]
    fn spacing_tightens_between_list_items_and_opens_before_headings() {
        assert!(gap(Kind::Item, Kind::Item) < gap(Kind::Text, Kind::Text));
        assert!(gap(Kind::Text, Kind::Heading) > gap(Kind::Text, Kind::Text));
        assert!(gap(Kind::Heading, Kind::Text) < gap(Kind::Text, Kind::Text));
    }
}
