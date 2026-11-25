//! The UI's icons: Phosphor's regular weight (MIT), cut down to the glyphs below and built in, so the desktop and a
//! browser draw the same ones without fetching anything (assets/fonts/README.md says where it came from). Icons stand
//! beside words where they help someone scan or recognize: the top bar's actions, the states of agents and tasks, and
//! the common actions of cards and menus. One weight and one size per place: [`SIZE`] beside UI text.
//!
//! The icons are a font family of their own, so no theme font can draw something else at their code points. A
//! context gets that family with the rest of its fonts ([`crate::theme::definitions`]); until it has it (a platform
//! still loading its fonts, or a test's context), every icon is left out and the words stand alone.

use egui::{Align2, Color32, FontFamily, FontId, Pos2, RichText};

/// The family's name, and the font in it.
pub const FAMILY: &str = "icons";
pub const DATA: &[u8] = include_bytes!("../../../assets/fonts/PhosphorIcons.ttf");

/// An icon's size beside UI text.
pub const SIZE: f32 = 14.0;

// The glyphs the font keeps. Adding one means cutting the font again with its code point (assets/fonts/README.md).
pub const BOARD: &str = "\u{eb54}";
pub const DIFF: &str = "\u{e27c}";
pub const TICKETS: &str = "\u{e490}";
pub const SETTINGS: &str = "\u{e272}";
pub const MORE: &str = "\u{e1fe}";
pub const NEW: &str = "\u{e3d4}";
pub const PROJECT: &str = "\u{e24a}";
pub const EXPLORER: &str = "\u{e67c}";
pub const ARCHIVE: &str = "\u{e00c}";
pub const APPROVE: &str = "\u{e182}";
pub const APPROVE_ALWAYS: &str = "\u{e53a}";
pub const CLOSE: &str = "\u{e4f6}";
pub const ATTACH: &str = "\u{e39a}";
pub const OPEN: &str = "\u{e5de}";
pub const NEEDS_YOU: &str = "\u{e4e2}";
pub const WORKING: &str = "\u{eb44}";
pub const DONE: &str = "\u{e184}";
pub const FAILED: &str = "\u{e4f8}";
pub const PLANNED: &str = "\u{e602}";
pub const BLOCKED: &str = "\u{e3a0}";
pub const REVIEW: &str = "\u{e220}";
pub const REMOTE: &str = "\u{e288}";
pub const SEND: &str = "\u{e396}";
pub const START: &str = "\u{e3d0}";
pub const PULL_REQUEST: &str = "\u{e282}";
pub const MENU: &str = "\u{e136}";
pub const DELETE: &str = "\u{e4a6}";
pub const REFRESH: &str = "\u{e036}";
pub const IDLE: &str = "\u{e18a}";

/// Every glyph above, for the test that the font has each of them.
pub const ALL: &[&str] = &[
    BOARD,
    DIFF,
    TICKETS,
    SETTINGS,
    MORE,
    NEW,
    PROJECT,
    EXPLORER,
    ARCHIVE,
    APPROVE,
    APPROVE_ALWAYS,
    CLOSE,
    ATTACH,
    OPEN,
    NEEDS_YOU,
    WORKING,
    DONE,
    FAILED,
    PLANNED,
    BLOCKED,
    REVIEW,
    REMOTE,
    SEND,
    START,
    PULL_REQUEST,
    MENU,
    DELETE,
    REFRESH,
    IDLE,
];

thread_local! {
    static READY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether this frame's context has the icons. Called as a frame begins, before anything draws one.
pub fn prepare(ctx: &egui::Context) -> bool {
    let ready = ctx.fonts(|fonts| fonts.families().contains(&FontFamily::Name(FAMILY.into())));
    READY.with(|cell| cell.set(ready));
    ready
}

/// Whether icons draw this frame.
pub fn ready() -> bool {
    READY.with(|cell| cell.get())
}

pub fn font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(FAMILY.into()))
}

/// `icon` as text of `size`, or nothing where this frame has no icons.
pub fn text(icon: &str, size: f32) -> Option<RichText> {
    ready().then(|| RichText::new(icon).font(font(size)))
}

/// `icon` and then `label`, as the parts of a button or a menu item; the label alone where this frame has no icons.
pub fn with<'a>(icon: &str, label: impl Into<egui::WidgetText>) -> egui::Atoms<'a> {
    let label: egui::WidgetText = label.into();
    match text(icon, SIZE) {
        Some(icon) => egui::Atoms::new((icon, label)),
        None => egui::Atoms::new(label),
    }
}

/// `icon` in `color` with its middle at `center`; nothing where this frame has no icons. Returns whether it drew.
pub fn paint(painter: &egui::Painter, center: Pos2, icon: &str, size: f32, color: Color32) -> bool {
    if ready() {
        painter.text(center, Align2::CENTER_CENTER, icon, font(size), color);
    }
    ready()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_font_has_every_icon_and_a_context_draws_them_once_it_has_the_family() {
        let ctx = egui::Context::default();
        let mut ready = true;
        ctx.run_ui(egui::RawInput::default(), |ui| ready = prepare(ui.ctx())).drop_without_applying_deltas();
        assert!(!ready, "egui's own fonts have no icons");
        assert!(text(BOARD, SIZE).is_none());
        ctx.set_fonts(crate::theme::definitions(None, None, |_| None));
        ctx.run_ui(egui::RawInput::default(), |ui| ready = prepare(ui.ctx())).drop_without_applying_deltas();
        assert!(ready);
        ctx.fonts_mut(|fonts| {
            for icon in ALL {
                let c = icon.chars().next().unwrap();
                assert!(fonts.has_glyph(&font(SIZE), c), "the cut font lacks U+{:04X}", c as u32);
            }
        });
        let ids: std::collections::HashSet<_> = ALL.iter().collect();
        assert_eq!(ids.len(), ALL.len(), "every icon is a glyph of its own");
    }
}
