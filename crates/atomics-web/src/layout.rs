//! The phone layout (design §4.4): chosen from the viewport's width and a coarse pointer, with a manual
//! override; one pane at a time, switched from a bottom bar; and only the part of the viewport the user
//! sees (`visualViewport`), so nothing sits under the soft keyboard or the screen's notch and corners. While
//! the user types into a terminal, the key bar takes the bottom bar's place: the page draws it on the
//! keyboard, from [`KEYBAR`], and says how tall it is.
//!
//! The phone layout draws zoomed by [`ZOOM`], so the text and controls the shared views size in points
//! come out right for a finger; its own measures are given in CSS pixels ([`px`]).

use egui::{Rect, pos2, vec2};

/// Narrower than this, in CSS pixels, is a phone whatever the pointer.
pub const NARROW: f32 = 600.0;
/// With a coarse pointer (a finger), narrower than this is too: a phone held sideways, a small tablet.
pub const COARSE_NARROW: f32 = 900.0;
/// How many CSS pixels a point is in the phone layout: the theme's UI size zooms egui by it.
pub const ZOOM: f32 = 1.25;

/// `css` CSS pixels in the phone layout's points.
pub const fn px(css: f32) -> f32 {
    css / ZOOM
}

pub const TOP_BAR: f32 = px(52.0);
pub const BOTTOM_BAR: f32 = px(56.0);
/// The least width and height of a touch target.
pub const TOUCH: f32 = px(44.0);
/// The space between the screen's sides (or its safe area's) and the content.
pub const GUTTER: f32 = px(16.0);

/// The user's choice, kept by the page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LayoutMode {
    /// From the viewport.
    #[default]
    Auto,
    Phone,
    Desktop,
}

impl LayoutMode {
    pub fn name(self) -> &'static str {
        match self {
            LayoutMode::Auto => "auto",
            LayoutMode::Phone => "phone",
            LayoutMode::Desktop => "desktop",
        }
    }

    pub fn named(name: &str) -> LayoutMode {
        match name {
            "phone" => LayoutMode::Phone,
            "desktop" => LayoutMode::Desktop,
            _ => LayoutMode::Auto,
        }
    }
}

/// Whether to draw the phone layout.
pub fn compact(width: f32, coarse: bool, mode: LayoutMode) -> bool {
    match mode {
        LayoutMode::Phone => true,
        LayoutMode::Desktop => false,
        LayoutMode::Auto => width < NARROW || (coarse && width < COARSE_NARROW),
    }
}

/// What the page says about the viewport: in CSS pixels as the page reports it, in points once
/// [`Viewport::points`] made it so.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Viewport {
    /// The part of the canvas the user sees; `None` before the page said.
    pub visible: Option<Rect>,
    /// The primary pointer is a finger.
    pub coarse: bool,
    /// The key bar's height while it shows, else 0.
    pub keybar: f32,
    /// The safe-area insets: the notch, the home indicator, and the sides of a phone held sideways.
    pub inset_top: f32,
    pub inset_bottom: f32,
    pub inset_left: f32,
    pub inset_right: f32,
}

impl Viewport {
    /// The page's report in points at egui's `zoom`.
    pub fn points(&self, zoom: f32) -> Viewport {
        let at = |v: f32| v / zoom.max(0.01);
        Viewport {
            visible: self
                .visible
                .map(|r| Rect::from_min_max(pos2(at(r.min.x), at(r.min.y)), pos2(at(r.max.x), at(r.max.y)))),
            coarse: self.coarse,
            keybar: at(self.keybar),
            inset_top: at(self.inset_top),
            inset_bottom: at(self.inset_bottom),
            inset_left: at(self.inset_left),
            inset_right: at(self.inset_right),
        }
    }
}

/// Where the phone layout's parts go, inside the safe area; their backgrounds reach the screen's sides.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Regions {
    pub top: Rect,
    pub body: Rect,
    /// The pane switcher, unless the key bar is up.
    pub bar: Option<Rect>,
    /// The space left for the page's key bar while typing.
    pub keybar: Option<Rect>,
}

pub fn regions(screen: Rect, viewport: &Viewport, typing: bool) -> Regions {
    let visible = viewport.visible.map(|v| v.intersect(screen)).filter(|v| v.width() > 0.0 && v.height() > 0.0);
    let visible = visible.unwrap_or(screen);
    // The notch is under the top of the layout viewport only; the keyboard never covers it.
    let inset_top = if visible.top() <= screen.top() { viewport.inset_top } else { 0.0 };
    let (left, right) = (visible.left() + viewport.inset_left, visible.right() - viewport.inset_right);
    let (left, right) = if right > left { (left, right) } else { (visible.left(), visible.right()) };
    let top = Rect::from_min_size(pos2(left, visible.top() + inset_top), vec2(right - left, TOP_BAR));
    let (bar, keybar, bottom) = if typing {
        let keybar = Rect::from_min_max(pos2(visible.left(), visible.bottom() - viewport.keybar), visible.max);
        (None, Some(keybar), keybar.top())
    } else {
        let inset = if visible.bottom() >= screen.bottom() { viewport.inset_bottom } else { 0.0 };
        let bar = Rect::from_min_max(pos2(left, visible.bottom() - BOTTOM_BAR - inset), pos2(right, visible.bottom()));
        (Some(bar), None, bar.top())
    };
    let body = Rect::from_min_max(pos2(left, top.bottom()), pos2(right, bottom.max(top.bottom())));
    Regions { top, body, bar, keybar }
}

/// The pane switcher's items, side by side across `bar` (its safe-area inset left below them).
pub fn bar_items(bar: Rect, count: usize) -> Vec<Rect> {
    let width = bar.width() / count.max(1) as f32;
    (0..count)
        .map(|i| Rect::from_min_size(pos2(bar.left() + i as f32 * width, bar.top()), vec2(width, BOTTOM_BAR)))
        .collect()
}

/// What a key bar key does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Send {
    /// A named key ([`crate::term_pane::named_key`]), or `Ctrl`/`Alt`, the sticky modifiers.
    Key(&'static str),
    Text(&'static str),
    /// The clipboard's text, read by the page when the key is pressed.
    Paste,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarKey {
    pub label: &'static str,
    /// What a screen reader says.
    pub name: &'static str,
    pub send: Send,
    /// Takes two slots.
    pub wide: bool,
}

const fn key(label: &'static str, name: &'static str, send: Send) -> BarKey {
    BarKey { label, name, send, wide: false }
}

/// The key bar, two rows of eight slots: the keys a phone's keyboard lacks.
pub const KEYBAR: [&[BarKey]; 2] = [
    &[
        key("Esc", "Escape", Send::Key("Escape")),
        key("Tab", "Tab", Send::Key("Tab")),
        key("Ctrl", "Control, held for the next key", Send::Key("Ctrl")),
        key("Alt", "Alt, held for the next key", Send::Key("Alt")),
        key("←", "Left", Send::Key("ArrowLeft")),
        key("↓", "Down", Send::Key("ArrowDown")),
        key("↑", "Up", Send::Key("ArrowUp")),
        key("→", "Right", Send::Key("ArrowRight")),
    ],
    &[
        key("PgUp", "Page up", Send::Key("PageUp")),
        key("PgDn", "Page down", Send::Key("PageDown")),
        key("|", "Pipe", Send::Text("|")),
        key("~", "Tilde", Send::Text("~")),
        key("/", "Slash", Send::Text("/")),
        key("-", "Dash", Send::Text("-")),
        BarKey { label: "Paste", name: "Paste", send: Send::Paste, wide: true },
    ],
];
/// Slots per row, the bar's padding and the gap between keys, and a key's least size, as the page lays them
/// out (`web/style.css`), in CSS pixels. A key is a finger's target both ways; a screen too narrow for eight
/// of them a row scrolls the row.
pub const KEYBAR_SLOTS: usize = 8;
pub const KEYBAR_PAD: f32 = 2.0;
pub const KEYBAR_GAP: f32 = 2.0;
pub const KEYBAR_KEY: f32 = 44.0;

/// One slot's width at a viewport `width`, with the safe area's side insets `inset` (each side's padding
/// is the larger of the inset and the bar's own).
pub fn keybar_slot(width: f32, (left, right): (f32, f32)) -> f32 {
    let inner = width - left.max(KEYBAR_PAD) - right.max(KEYBAR_PAD);
    ((inner - (KEYBAR_SLOTS - 1) as f32 * KEYBAR_GAP) / KEYBAR_SLOTS as f32).max(KEYBAR_KEY)
}

/// The key bar for the page: `[[{label, name, kind, value, wide}]]`.
pub fn keybar_json() -> serde_json::Value {
    let rows: Vec<serde_json::Value> = KEYBAR
        .iter()
        .map(|row| {
            row.iter()
                .map(|k| {
                    let (kind, value) = match k.send {
                        Send::Key(name) => ("key", name),
                        Send::Text(text) => ("text", text),
                        Send::Paste => ("paste", ""),
                    };
                    serde_json::json!({ "label": k.label, "name": k.name, "kind": kind, "value": value, "wide": k.wide })
                })
                .collect()
        })
        .collect();
    serde_json::Value::Array(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_phone_layout_follows_the_width_and_the_pointer_unless_the_user_chose() {
        for width in [360.0, 390.0, 430.0, 599.0] {
            assert!(compact(width, false, LayoutMode::Auto), "{width}");
        }
        assert!(!compact(820.0, false, LayoutMode::Auto), "a narrow desktop window with a mouse");
        assert!(compact(820.0, true, LayoutMode::Auto), "a tablet held upright");
        assert!(!compact(1180.0, true, LayoutMode::Auto), "a tablet held sideways");
        assert!(!compact(390.0, true, LayoutMode::Desktop) && compact(1440.0, false, LayoutMode::Phone));
        for mode in [LayoutMode::Auto, LayoutMode::Phone, LayoutMode::Desktop] {
            assert_eq!(LayoutMode::named(mode.name()), mode);
        }
        assert_eq!(LayoutMode::named("tablet"), LayoutMode::Auto);
    }

    #[test]
    fn the_regions_fit_the_visible_viewport_at_phone_widths() {
        for (width, height) in [(360.0, 780.0), (390.0, 844.0), (430.0, 932.0)] {
            let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(width, height));
            let port = Viewport {
                visible: Some(screen),
                coarse: true,
                keybar: 88.0,
                inset_top: 47.0,
                inset_bottom: 34.0,
                ..Viewport::default()
            };
            let r = regions(screen, &port, false);
            assert_eq!((r.top.top(), r.top.width()), (47.0, width), "under the notch, full width");
            let bar = r.bar.expect("the switcher");
            assert_eq!(bar.bottom(), height);
            assert!((bar.height() - (BOTTOM_BAR + 34.0)).abs() < 1e-3, "above the home indicator: {bar:?}");
            assert_eq!((r.body.width(), r.body.top(), r.body.bottom()), (width, r.top.bottom(), bar.top()));
            for item in bar_items(bar, 5) {
                assert!(item.width() >= TOUCH && screen.contains_rect(item), "{width}: {item:?}");
            }
            // The keyboard is up: the visible part shrinks, and the key bar sits at its bottom.
            let keyboard = Viewport { visible: Some(screen.with_max_y(height - 336.0)), ..port };
            let r = regions(screen, &keyboard, true);
            let keys = r.keybar.expect("room for the key bar");
            assert!(r.bar.is_none());
            assert_eq!((keys.bottom(), keys.height()), (height - 336.0, 88.0));
            assert_eq!((r.body.bottom(), r.body.width()), (keys.top(), width), "the terminal fills the width above it");
            // Every key bar key is a finger's target, 44 CSS pixels each way, two rows of eight slots; they fit
            // without scrolling from an iPhone mini's width up.
            assert!(keybar_slot(width, (0.0, 0.0)) >= KEYBAR_KEY, "{width}");
            let fits = |width: f32| width - 2.0 * KEYBAR_PAD - 7.0 * KEYBAR_GAP >= 8.0 * KEYBAR_KEY;
            assert_eq!(fits(width), width >= 375.0, "{width}");
            for row in KEYBAR {
                let slots: usize = row.iter().map(|k| if k.wide { 2 } else { 1 }).sum();
                assert_eq!(slots, KEYBAR_SLOTS);
            }
        }
    }

    #[test]
    fn a_phone_held_sideways_keeps_to_its_safe_area_and_the_page_speaks_in_css_pixels() {
        let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(844.0, 390.0));
        let port = Viewport {
            visible: Some(screen),
            coarse: true,
            inset_left: 47.0,
            inset_right: 47.0,
            inset_bottom: 21.0,
            ..Viewport::default()
        };
        let r = regions(screen, &port, false);
        let bar = r.bar.unwrap();
        for part in [r.top, r.body, bar] {
            assert_eq!((part.left(), part.right()), (47.0, 797.0), "{part:?}: clear of the notch and the corners");
        }
        assert_eq!(bar.bottom(), 390.0);
        // The page reports CSS pixels; zoomed, they are fewer points.
        let zoomed = port.points(ZOOM);
        assert_eq!(zoomed.visible, Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(844.0 / ZOOM, 390.0 / ZOOM))));
        assert_eq!((zoomed.inset_left, zoomed.inset_bottom), (47.0 / ZOOM, 21.0 / ZOOM));
        assert!(zoomed.coarse);
        let css = |points: f32| (points * ZOOM * 100.0).round() / 100.0;
        assert_eq!((css(TOUCH), css(GUTTER)), (44.0, 16.0), "a finger's target, and the side gutter, in CSS pixels");
        // Held sideways, the key bar keeps its keys out of the notch's side.
        let sideways = keybar_slot(844.0, (47.0, 47.0));
        assert!((sideways - (844.0 - 94.0 - 14.0) / 8.0).abs() < 1e-3 && sideways >= KEYBAR_KEY, "{sideways}");
    }

    #[test]
    fn a_page_scrolled_by_the_keyboard_keeps_the_top_bar_in_view() {
        let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(390.0, 844.0));
        let visible = Rect::from_min_size(pos2(0.0, 120.0), vec2(390.0, 400.0));
        let port = Viewport { visible: Some(visible), keybar: 88.0, inset_top: 47.0, ..Viewport::default() };
        let r = regions(screen, &port, true);
        assert_eq!(r.top.top(), 120.0, "no notch inset once the page moved under it");
        assert_eq!(r.keybar.unwrap().bottom(), 520.0);
    }

    #[test]
    fn the_key_bar_goes_to_the_page_as_it_is_defined() {
        let json = keybar_json();
        assert_eq!(json[0][0]["value"], "Escape");
        assert_eq!(json[0][2]["value"], "Ctrl");
        assert_eq!(
            json[1][2],
            serde_json::json!({ "label": "|", "name": "Pipe", "kind": "text", "value": "|", "wide": false })
        );
        assert_eq!((json[1][6]["kind"].as_str(), json[1][6]["wide"].as_bool()), (Some("paste"), Some(true)));
        let named: Vec<&str> = KEYBAR
            .iter()
            .flat_map(|row| row.iter())
            .filter_map(|k| if let Send::Key(n) = k.send { Some(n) } else { None })
            .collect();
        for name in named.into_iter().filter(|n| !matches!(*n, "Ctrl" | "Alt")) {
            assert!(crate::term_pane::named_key(name).is_some(), "{name} is a key the pane knows");
        }
    }
}
