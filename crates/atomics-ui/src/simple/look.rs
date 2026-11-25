//! Simple mode's type and ink: one scale of sizes and weights for every surface, and text colours lifted from the
//! theme until they read on its surfaces, so a custom theme stays legible too.

use egui::text::{LayoutJob, TextFormat};
use egui::{Align2, Color32, Pos2, Rect, RichText};

use crate::flow::font;
use crate::theme::Palette;

pub const CODEX_LILAC: Color32 = Color32::from_rgb(179, 153, 252);
pub const CODEX_INK: Color32 = Color32::from_rgb(38, 24, 63);

/// Shared surface for the composer, agent rows and board cards. The edge stays visible above the shadow in dark themes.
pub fn card(p: &Palette) -> egui::Frame {
    let light = p.mode == atomics_core::theme::ThemeMode::Light;
    egui::Frame::new()
        .fill(crate::theme::lerp(p.raised, Color32::WHITE, if light { 0.60 } else { 0.035 }))
        .stroke(egui::Stroke::new(1.0, crate::theme::lerp(p.border, p.foreground, if light { 0.06 } else { 0.12 })))
        .corner_radius(8)
        .shadow(egui::Shadow {
            offset: [0, 4],
            blur: 10,
            spread: 0,
            color: Color32::from_black_alpha(if light { 32 } else { 120 }),
        })
}

/// A low shadow and a quiet outline for the message field and user prompts.
pub fn chat_surface(p: &Palette) -> egui::Frame {
    let light = p.mode == atomics_core::theme::ThemeMode::Light;
    egui::Frame::new()
        .corner_radius(8)
        .fill(p.raised)
        .stroke(egui::Stroke::new(1.0, crate::theme::lerp(p.raised, p.border, 0.65)))
        .shadow(egui::Shadow {
            offset: [0, 1],
            blur: 3,
            spread: 0,
            color: Color32::from_black_alpha(if light { 6 } else { 24 }),
        })
}

/// Directional edge light and a contact shadow give the surface a visible lip at native scale.
pub fn surface_edges(painter: &egui::Painter, rect: Rect, p: &Palette) {
    if rect.width() <= 18.0 {
        return;
    }
    let edge = rect.left() + 8.0..=rect.right() - 8.0;
    let light = p.mode == atomics_core::theme::ThemeMode::Light;
    painter.hline(
        edge.clone(),
        rect.top() + 1.0,
        egui::Stroke::new(1.0, Color32::from_white_alpha(if light { 175 } else { 39 })),
    );
    painter.hline(
        edge,
        rect.bottom() - 0.5,
        egui::Stroke::new(1.0, Color32::from_black_alpha(if light { 22 } else { 105 })),
    );
}

/// Dialogs inherit the theme's stronger window shadow above the board.
pub fn dialog(ui: &egui::Ui, p: &Palette) -> egui::Frame {
    card(p).fill(p.overlay.to_opaque()).corner_radius(12).shadow(ui.visuals().window_shadow)
}

/// The welcome's question.
pub const DISPLAY: f32 = 24.0;
/// A plan's or a saved chat's title.
pub const TITLE: f32 = 19.0;
/// Running text: the conversation, the user's messages and the composer.
pub const BODY: f32 = 16.0;
/// Controls and list rows: buttons, the sidebar's rows, a card's rows.
pub const LABEL: f32 = 14.0;
/// Section labels, metadata and states.
pub const SMALL: f32 = 12.0;
/// Code inside running text: a monospace face reads larger, so a step under [`BODY`].
pub const CODE: f32 = 13.5;
/// A code block's lines.
pub const CODE_BLOCK: f32 = 13.0;

/// Running text's line height, in points: 1.55 of [`BODY`], on whole points so rows stay crisp.
pub fn leading(size: f32) -> f32 {
    (size * 1.55).round()
}

/// Weights on the variable faces: Geist Sans on desktop and web.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weight {
    Regular,
    /// Section labels and the names of things.
    Medium,
    /// Titles and bold text.
    Semibold,
}

impl Weight {
    /// The face's `wght` axis for this weight; none for the regular face.
    pub fn axis(self) -> Option<f32> {
        match self {
            Weight::Regular => None,
            Weight::Medium => Some(530.0),
            Weight::Semibold => Some(620.0),
        }
    }
}

/// Text in simple mode's face at `size` and `weight`.
pub fn text(words: impl Into<String>, size: f32, weight: Weight, color: Color32) -> RichText {
    let text = RichText::new(words).font(font(size)).color(color);
    match weight.axis() {
        Some(wght) => text.variation("wght", wght),
        None => text,
    }
}

/// Text painted at `pos`, placed by `align`, in simple mode's face at `size` and `weight`; returns where it went.
pub fn paint(
    painter: &egui::Painter,
    pos: Pos2,
    align: Align2,
    words: &str,
    size: f32,
    weight: Weight,
    color: Color32,
) -> Rect {
    let mut format = TextFormat { font_id: font(size), color, ..TextFormat::default() };
    if let Some(wght) = weight.axis() {
        format.coords.push("wght", wght);
    }
    let galley = painter.layout_job(LayoutJob::single_section(words.to_owned(), format));
    let rect = align.anchor_size(pos, galley.size());
    painter.galley(rect.min, galley, color);
    rect
}

/// How far each text colour stands from the theme's surfaces, as a WCAG contrast ratio.
pub const SECONDARY_CONTRAST: f32 = 7.0;
pub const TERTIARY_CONTRAST: f32 = 4.6;

/// Text colours for simple mode: what the user reads, what explains it, and what only labels it. The secondary and
/// tertiary inks are the theme's muted and quiet colours, lifted until they reach their contrast on the theme's
/// lightest surface.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ink {
    pub primary: Color32,
    pub secondary: Color32,
    pub tertiary: Color32,
}

impl Ink {
    pub fn of(p: &Palette) -> Ink {
        Ink {
            primary: p.foreground,
            secondary: p.simple_text(p.muted, SECONDARY_CONTRAST),
            tertiary: p.simple_text(p.quiet, TERTIARY_CONTRAST),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contrast(a: Color32, b: Color32) -> f32 {
        let luminance = |c: Color32| {
            let [r, g, b, _] = egui::Rgba::from(c.to_opaque()).to_array();
            0.2126 * r + 0.7152 * g + 0.0722 * b
        };
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn every_ink_reads_on_every_simple_surface_in_each_built_in_theme() {
        for name in atomics_core::theme::builtin_names() {
            let tokens = atomics_core::theme::builtin(name).unwrap();
            let p = Palette::new(&tokens, Default::default());
            let ink = Ink::of(&p);
            for surface in [p.background, p.panel, p.sidebar] {
                assert!(contrast(ink.primary, surface) >= 10.0, "{name}: primary on {surface:?}");
                assert!(contrast(ink.secondary, surface) >= SECONDARY_CONTRAST, "{name}: secondary on {surface:?}");
                assert!(contrast(ink.tertiary, surface) >= TERTIARY_CONTRAST, "{name}: tertiary on {surface:?}");
            }
        }
    }

    #[test]
    fn running_text_sits_on_whole_point_rows_and_titles_weigh_more_than_text() {
        assert_eq!(leading(BODY), 25.0);
        const { assert!(DISPLAY > TITLE && TITLE > BODY && BODY > LABEL && LABEL > SMALL) };
        assert!(Weight::Semibold.axis() > Weight::Medium.axis() && Weight::Regular.axis().is_none());
    }
}
