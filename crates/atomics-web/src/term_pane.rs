//! A terminal in the browser: the host's grid, never resized by looking at it, with the input the
//! `<textarea>` overlay forwards (design §4.4). The views draw terminals where the shared views put them,
//! through [`Terminals`], the browser's `TerminalSurface`: the one the page's input goes to with this pane's
//! overlay, every other as it is ([`TermPane::show`]), where a click picks it. The page puts the overlay over
//! the rect this pane says ([`Overlay::rect`]); everything typed, every key-bar key and every touch there
//! arrives here as a [`TermInput`], is encoded with the screen's latest modes
//! (`atomics_ui::terminal::encode`) and typed under the lease.
//!
//! - **The grid is the desktop's.** The pane pans and zooms it: the font fits the width until the user
//!   pinches, but on a phone never below a readable size, so a wider grid pans sideways; a grid taller than
//!   the pane keeps the cursor in view, and the terminal's background fills the rest of the pane. "Fit to
//!   this screen" is an explicit resize to the pane at that font, shown on the desktop, that lasts as long as
//!   the lease, which the pane keeps while it shows the terminal.
//! - **Where there is no desktop** (`RemoteBackend::fits`), a native client gives each terminal its pane's size
//!   at the desktop's font, as the desktop does with its own panes ([`TermPane::fit`]), and follows the pane as it
//!   changes. The pane the input goes to and every other keep the same room for the strip saying who types, so
//!   picking a terminal never resizes it.
//! - **Touch.** A tap focuses the overlay (the browser opens the keyboard) and, where the program takes
//!   mouse reports and this client types, clicks the cell; a drag scrolls history (this client's view
//!   only) and pans; a long press selects the word under the finger, with a handle at each end to drag
//!   and Copy for just that range; a pinch sets the font size. A mouse clicks and drag-selects; its wheel
//!   scrolls.
//! - **The overlay yields to what lies over it.** It takes the pointer only where egui would give the
//!   terminal the pointer: a floating window over the terminal is cut out of it ([`Overlay::covered`]), and an
//!   open menu or popup takes all of it, so a click away closes the popup as it does anywhere else. A click
//!   outside it, on a text box the page draws, takes the browser's focus with it, so what is typed there goes
//!   there and nothing reaches the terminal.
//! - **Typing needs the lease**, asked for only when the overlay gains the focus (a tap or click, or a pick that
//!   moves it there) and only where `welcome` grants `type`. Losing it to the desktop or another client, or
//!   being refused it, blurs the overlay, so typing again is a new tap. Text, keys and pastes forwarded while the
//!   overlay does not have the focus, or in the frame the lease was lost, are dropped: what was in flight
//!   then (keys of that frame, a word the keyboard commits as the overlay is blurred, a Paste whose clipboard read
//!   finishes late) neither types nor asks for the lease again. A word being composed when the user clicks away
//!   commits first, so it is typed, with no Return. The input leaving a terminal lets go of its lease, and so
//!   does the keyboard going to a text box or the panel.
//! - **Offline** the screen shows as it was, marked disconnected, and nothing is typed.

use std::collections::HashMap;

use atomics_core::lease::{GridSize, MAX_CELL_PIXELS};
use atomics_core::ops::Grant;
use atomics_core::screen::{self as grid, Modes, Screen};
use atomics_ui::Backend;
use atomics_ui::terminal::{self as painter, Metrics, Point, Selection, encode};
use atomics_ui::terminal_surface::TerminalSurface;
use atomics_ui::theme::Palette;
use atomics_ui::touch::{self, Touch};
use atomics_ui::window_keys;
use egui::{Align2, Key, LayerId, Modifiers, Order, Pos2, Rect, RichText, Sense, Stroke, Vec2, pos2, vec2};

use crate::gesture::{Gesture, Kind, Phase, Pointer, Recognizer};
use crate::remote::RemoteBackend;
use crate::screens::{Lease, Lost, MAX_WAITING_BYTES, Refused};

/// The font size range a pinch moves in, and the fitted default's.
pub const MIN_FONT: f32 = 5.0;
pub const MAX_FONT: f32 = 28.0;
const FIT_MIN: f32 = 7.0;
const FIT_MAX: f32 = 14.0;
/// On a phone the fitted font stays readable: a wider grid pans instead.
const PHONE_FIT_MIN: f32 = 13.0;
/// Space between the pane's edge and the cells; on a phone the cells keep to the screen's gutter.
const PAD: f32 = 4.0;
/// How often the lease is renewed while the user stays at the terminal without typing, or while its size is in force.
const RENEW_MS: f64 = 10_000.0;
/// A selection handle's knob, and how far from its centre a finger still takes it.
const KNOB: f32 = 6.0;
const KNOB_REACH: f32 = 24.0;

/// What the overlay forwards.
#[derive(Debug, Clone, PartialEq)]
pub enum TermInput {
    /// Committed text: typed, dictated or composed.
    Text(String),
    /// The composition in progress; empty when it ends.
    Preedit(String),
    /// A named key (`Escape`, `ArrowUp`, `PageDown`, …, or `Ctrl` and `Alt`, the key bar's sticky
    /// modifiers), or one character with Ctrl from a hardware keyboard.
    Key {
        name: String,
        mods: Modifiers,
    },
    Paste(String),
    Focus(bool),
    Pointer(Pointer),
    /// A wheel over the terminal, in points; `zoom` when Ctrl was held.
    Wheel {
        delta: Vec2,
        at: Pos2,
        zoom: bool,
    },
    /// The page copied the selection.
    Copied,
}

/// What the page should make of the overlay after a frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overlay {
    /// Over the terminal's cells, while one shows.
    pub rect: Option<Rect>,
    /// The parts of `rect` that egui's layers above the terminal take the pointer over ([`clip_path`]).
    pub covered: Vec<Rect>,
    /// May be typed into: a device that may type, connected, and nobody else typing.
    pub writable: bool,
    /// The key bar's sticky modifiers.
    pub ctrl: bool,
    pub alt: bool,
    /// A selection to offer Copy for, and where: above the point, or below the height where there is no
    /// room above it in the terminal.
    pub copy: Option<(String, Pos2, f32)>,
    /// Take the focus away: the lease went elsewhere, or the input goes to another terminal now.
    pub blur: bool,
    /// Give it the focus where a pointer that is not a finger picked this terminal: a click into it types
    /// there, as on the desktop. A finger taps the overlay itself, which opens the keyboard.
    pub focus: bool,
    /// The terminal is an editor's, which keeps Shift with the arrows ([`atomics_ui::window_keys`]).
    pub editor: bool,
}

/// One line and column of the terminal: `line` is a line ID (history below the screen's first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Cell {
    line: u64,
    col: u16,
}

/// A finger dragging one end of the selection: the pointer, the other end, which stays where it is even when
/// the finger crosses it, and how far the dragged end's cell centre was from the finger when it took the handle.
#[derive(Debug, Clone, Copy, PartialEq)]
struct HandleDrag {
    id: i32,
    anchor: Cell,
    grab: Vec2,
}

/// A character a word holds, for a long press: letters, digits and what paths and names are made of.
fn word_char(c: char) -> bool {
    c.is_alphanumeric() || "_-./~:@%+".contains(c)
}

/// A terminal pane's own state: the view (font, panning, history), the selection, the composition, the
/// sticky modifiers and the gestures in progress.
pub struct TermPane {
    session: Option<String>,
    /// The font size the user pinched to; `None` fits the width.
    pub font: Option<f32>,
    pan_x: f32,
    /// The top line while scrolled back; `None` follows the screen.
    top: Option<u64>,
    scroll_rest: f32,
    selection: Option<(Cell, Cell)>,
    /// The selection was made by touch: it shows its handles.
    handles: bool,
    dragging: Option<HandleDrag>,
    preedit: String,
    ctrl: bool,
    alt: bool,
    focused: bool,
    held: bool,
    gestures: Recognizer,
    view: Screen,
    /// When the lease was last used or renewed.
    active_ms: f64,
    /// What the overlay forwarded since the last frame.
    pub inputs: Vec<TermInput>,
    pub overlay: Overlay,
    /// This frame drew a terminal.
    drawn: bool,
    /// The phone layout's measures while it draws, set by the views each frame.
    pub touch: Option<Touch>,
    /// The desktop's terminal font size where this client gives the terminal its pane's size, set by the views
    /// each frame; `None` shows the host's grid as it is.
    pub fit: Option<f32>,
    /// This client holds the lease with its own size in force.
    sized: bool,
    /// The overlay is to lose the focus at the next draw: the input went to another terminal.
    blur_next: bool,
    /// The overlay is to take the focus at the next draw ([`Overlay::focus`]).
    pub focus_next: bool,
    /// What was typed while the panel had the keyboard, which goes to this terminal at the next draw.
    handed: Vec<TermInput>,
    /// Where the cells were last painted.
    painted: Option<Frame>,
    /// The layer the terminal was last drawn in.
    layer: LayerId,
}

impl Default for TermPane {
    fn default() -> Self {
        Self {
            session: None,
            font: None,
            pan_x: 0.0,
            top: None,
            scroll_rest: 0.0,
            selection: None,
            handles: false,
            dragging: None,
            preedit: String::new(),
            ctrl: false,
            alt: false,
            focused: false,
            held: false,
            gestures: Recognizer::default(),
            view: Screen::blank(0, 0),
            active_ms: 0.0,
            inputs: Vec::new(),
            overlay: Overlay::default(),
            drawn: false,
            touch: None,
            fit: None,
            sized: false,
            blur_next: false,
            focus_next: false,
            handed: Vec::new(),
            painted: None,
            layer: LayerId::background(),
        }
    }
}

/// The geometry of one frame: where the cells are and which lines they show.
#[derive(Debug, Clone, Copy)]
struct Frame {
    rect: Rect,
    /// Between the pane's edges and the cells.
    pad: Vec2,
    origin: Pos2,
    /// The font size in use, and its cell.
    size: f32,
    cell: Vec2,
    /// The live screen's first line.
    first: u64,
    top: u64,
    rows: u16,
    cols: u16,
    live_top: u64,
    oldest: u64,
}

impl Frame {
    fn cell_at(&self, pos: Pos2) -> Cell {
        let col = ((pos.x - self.origin.x) / self.cell.x).floor().clamp(0.0, f32::from(self.cols.max(1) - 1));
        let row = ((pos.y - self.origin.y) / self.cell.y).floor().clamp(0.0, f32::from(self.rows.max(1) - 1));
        Cell { line: self.top + row as u64, col: col as u16 }
    }

    /// The live screen's row of `cell`, if it is on the live screen.
    fn screen_row(&self, cell: Cell) -> Option<u16> {
        cell.line.checked_sub(self.first).map(|row| row as u16)
    }
}

/// Whether `key`'s name is one the overlay sends.
pub fn named_key(name: &str) -> Option<Key> {
    Some(match name {
        "Escape" => Key::Escape,
        "Tab" => Key::Tab,
        "Enter" => Key::Enter,
        "Backspace" => Key::Backspace,
        "Delete" => Key::Delete,
        "Insert" => Key::Insert,
        "ArrowUp" => Key::ArrowUp,
        "ArrowDown" => Key::ArrowDown,
        "ArrowLeft" => Key::ArrowLeft,
        "ArrowRight" => Key::ArrowRight,
        "Home" => Key::Home,
        "End" => Key::End,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        "F1" => Key::F1,
        "F2" => Key::F2,
        "F3" => Key::F3,
        "F4" => Key::F4,
        "F5" => Key::F5,
        "F6" => Key::F6,
        "F7" => Key::F7,
        "F8" => Key::F8,
        "F9" => Key::F9,
        "F10" => Key::F10,
        "F11" => Key::F11,
        "F12" => Key::F12,
        _ => return None,
    })
}

/// What a key from the overlay sends, with the sticky modifiers added to its own.
pub fn key_text(name: &str, mods: Modifiers, (ctrl, alt): (bool, bool), modes: &Modes) -> Option<String> {
    // A keyboard's Ctrl with a digit is the window's, on every client (`atomics_ui::window_keys`): Ctrl+3 would be
    // Escape here. The key bar's sticky Ctrl types its control byte as text.
    if mods.ctrl && name.len() == 1 && name.as_bytes()[0].is_ascii_digit() {
        return None;
    }
    let mods = Modifiers { ctrl: mods.ctrl || ctrl, alt: mods.alt || alt, ..mods };
    if let Some(key) = named_key(name) {
        return encode::key(key, mods, modes).and_then(|bytes| String::from_utf8(bytes).ok());
    }
    let mut chars = name.chars();
    let (Some(_), None) = (chars.next(), chars.next()) else { return None };
    Some(encode::text(name, mods.ctrl, mods.alt))
}

impl TermPane {
    /// Start over for another terminal: the view follows it; the font stays. Typing in the last one ends:
    /// the overlay loses the focus, and what it forwarded since is dropped.
    fn switch(&mut self, session: &str) {
        if self.session.as_deref() != Some(session) {
            self.session = Some(session.to_string());
            self.top = None;
            self.pan_x = 0.0;
            self.scroll_rest = 0.0;
            self.selection = None;
            self.dragging = None;
            self.preedit.clear();
            self.held = false;
            self.gestures = Recognizer::default();
            self.inputs.clear();
            self.let_go();
        }
    }

    /// Takes from what the overlay forwarded the keys that are the window's, as the input events they are: the overlay
    /// has the page's keyboard, but they never reach a terminal.
    pub fn window_keys(&mut self, os: egui::os::OperatingSystem) -> Vec<egui::Event> {
        let editor = self.overlay.editor;
        let mut keys = Vec::new();
        self.inputs.retain(|input| {
            let TermInput::Key { name, mods } = input else { return true };
            let key = named_key(name).or_else(|| Key::from_name(name).filter(|key| atomics_ui::keys::is_digit(*key)));
            let Some(key) = key else { return true };
            let event =
                egui::Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers: *mods };
            let windows = window_keys::before_terminal(&event, os, editor);
            if windows {
                keys.push(event);
            }
            !windows
        });
        keys
    }

    /// The panel gave the keyboard back to this terminal: the overlay takes it at the next draw, and `typed`, what was
    /// typed in the panel to do that, reaches the terminal instead of being lost.
    pub fn hand_back(&mut self, typed: Vec<TermInput>) {
        self.focus_next = true;
        self.handed = typed;
    }

    /// The keyboard moved to something else the page draws: the overlay gives it up on the next frame.
    pub fn let_go(&mut self) {
        self.blur_next |= std::mem::take(&mut self.focused);
    }

    /// The terminal this pane follows, by session id.
    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    /// The centre of the cell `row` lines under the top of what was last painted, at `col`, and a cell's size.
    #[cfg(test)]
    fn cell_centre(&self, row: u16, col: u16) -> (Pos2, Vec2) {
        let frame = self.painted.expect("painted");
        (frame.origin + vec2((f32::from(col) + 0.5) * frame.cell.x, (f32::from(row) + 0.5) * frame.cell.y), frame.cell)
    }

    /// Whether the overlay has the focus: the user is at the keyboard.
    pub fn typing(&self) -> bool {
        self.focused
    }

    pub fn begin_frame(&mut self) {
        self.drawn = false;
    }

    /// After a frame: what lies over the terminal takes its part of the overlay. A frame that drew no
    /// terminal takes the overlay away, the focus and the lease with it, and what it forwarded is dropped,
    /// never typed later.
    pub fn end_frame(&mut self, ctx: &egui::Context, remote: &RemoteBackend) {
        if self.drawn {
            if let Some(rect) = self.overlay.rect {
                self.overlay.covered = covered(ctx, self.layer, rect);
            }
            // The keyboard is in a text box the page draws or in the panel: another browser would see the lease held
            // until it expires, by someone who has moved on. A click back asks again.
            let elsewhere = ctx.output(|output| output.ime.is_some())
                || ctx.memory(|m| m.has_focus(atomics_ui::panel_nav::focus_id()));
            if self.held && !self.focused && elsewhere {
                if let Some(session) = &self.session {
                    remote.release(session);
                }
                self.held = false;
            }
            return;
        }
        if let Some(last) = &self.session {
            remote.release(last);
        }
        self.drop_inputs();
        self.handed.clear();
        self.preedit.clear();
        let blur = std::mem::take(&mut self.focused);
        self.overlay = Overlay { blur, ..Overlay::default() };
    }

    /// Drop what the overlay forwarded, and the gestures in progress with it: a touch whose lift is
    /// dropped would otherwise stay down, and make every later touch a pinch.
    fn drop_inputs(&mut self) {
        self.inputs.clear();
        self.gestures = Recognizer::default();
    }

    /// When the pane wants a frame without input: a long press coming due, or a lease to renew.
    pub fn due_ms(&self) -> Option<f64> {
        let renew = (self.held && (self.focused || self.sized)).then_some(self.active_ms + RENEW_MS);
        self.gestures.due_ms().into_iter().chain(renew).reduce(f64::min)
    }

    /// Draw `session`'s terminal as it is into `rect`, for a view that shows it while the page's input goes
    /// to another: no header, no overlay, the live screen at the fitted size.
    pub fn show(&mut self, ui: &mut egui::Ui, rect: Rect, session: &str, remote: &RemoteBackend, p: &Palette) {
        self.switch(session);
        remote.show_screen(session, false);
        // Fitted, it keeps the room the strip takes where the input goes, so picking it never resizes it.
        let rect = match self.fit {
            Some(_) => {
                let strip = rect.with_max_y(rect.top() + self.header_height());
                ui.painter().rect_filled(strip, 0.0, p.panel);
                rect.with_min_y(strip.bottom())
            }
            None => rect,
        };
        match self.frame(ui, rect, session, remote, p) {
            Some(frame) => {
                self.refit(&frame, session, remote);
                self.paint(ui, &frame, session, remote, p, false)
            }
            None => waiting(ui, rect, remote.live(), p),
        }
    }

    /// The strip over the terminal the input goes to, which says who types.
    fn header_height(&self) -> f32 {
        self.touch.map_or(24.0, |touch| touch.target + 8.0)
    }

    /// Where this client gives the terminal its size ([`TermPane::fit`]): the grid the pane holds, whenever that
    /// changes.
    fn refit(&self, frame: &Frame, session: &str, remote: &RemoteBackend) {
        if self.fit.is_some() {
            remote.fit(session, fit_size(frame));
        }
    }

    /// Draw `session`'s terminal into `rect`, under a strip saying who types, after carrying out what the
    /// overlay forwarded. The tile around it draws the session's header and the approval it waits on.
    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        session: &str,
        remote: &RemoteBackend,
        p: &Palette,
        now_ms: f64,
    ) {
        // The input leaves the terminal it was on, and so does the lease: another browser would see it held
        // until it expires, by someone who has moved on.
        if let Some(last) = self.session.as_deref().filter(|last| *last != session) {
            remote.release(last);
        }
        self.switch(session);
        self.drawn = true;
        self.layer = ui.layer_id();
        remote.show_screen(session, true);
        let lease = remote.lease(session);
        let may_type = remote.may(Grant::Type);
        let live = remote.live();

        // Header: who types, and what this client may do about it.
        let height = self.header_height();
        let margin = self.touch.map_or(vec2(8.0, 2.0), |touch| vec2(touch.gutter, 4.0));
        let header = Rect::from_min_size(rect.min, vec2(rect.width(), height));
        ui.painter().rect_filled(header, 0.0, p.panel);
        let mut actions = Actions::default();
        let mut child =
            ui.new_child(egui::UiBuilder::new().id_salt(("term-header", session)).max_rect(header.shrink2(margin)));
        child.horizontal_centered(|ui| self.header(ui, &lease, may_type, live, p, &mut actions));
        let body = rect.with_min_y(header.bottom());

        let Some(frame) = self.frame(ui, body, session, remote, p) else {
            waiting(ui, body, live, p);
            self.drop_inputs();
            let blur = std::mem::take(&mut self.focused) || std::mem::take(&mut self.blur_next);
            self.overlay = Overlay { blur, ..Overlay::default() };
            return;
        };

        // Losing the lease ends this round of typing, before anything the overlay forwarded since is
        // carried out: the overlay loses the focus, so that text in flight is dropped (see the module's
        // notes), and typing again is a new tap.
        let lost = self.held && !lease.yours;
        if lost {
            self.focused = false;
            self.held = false;
            self.preedit.clear();
            (self.ctrl, self.alt) = (false, false);
        }
        let modes = remote.with_screen(session, |s| s.map(|(screen, _)| screen.modes)).unwrap_or_default();
        let mut typed = String::new();
        let was_focused = self.focused;
        for input in std::mem::take(&mut self.inputs) {
            self.input(input, &frame, &modes, &lease, lost, remote, session, &mut typed, now_ms);
        }
        // The first key after the panel had the keyboard: the overlay takes the focus after this frame, and the lease
        // is asked for now, so this key waits for it like one typed right after a click.
        let handed = std::mem::take(&mut self.handed);
        if !handed.is_empty() && live && may_type && (lease.yours || lease.holder.is_none()) {
            if !lease.yours {
                remote.acquire(session, false);
            }
            self.focused = true;
            self.active_ms = now_ms;
            for input in handed {
                self.input(input, &frame, &modes, &lease, lost, remote, session, &mut typed, now_ms);
            }
        }
        // The overlay takes the pointer before egui sees it: a text box that had the keyboard learns here.
        if self.focused && !was_focused {
            ui.memory_mut(|m| m.focused().into_iter().for_each(|id| m.surrender_focus(id)));
        }
        for gesture in self.gestures.tick(now_ms) {
            self.gesture(gesture, &frame, &modes, &lease, &mut typed);
        }
        if !typed.is_empty() && live && may_type {
            self.top = None;
            match remote.type_text(session, &typed) {
                Ok(()) => {}
                Err(Refused::TooMuch(limit)) => remote.say(too_much(limit)),
                Err(Refused::NoLease) if lease.holder.is_some() && !lease.yours => {
                    remote.say("Someone else is typing in this terminal. Take over to type here.")
                }
                Err(Refused::NoLease) => {}
            }
            self.active_ms = now_ms;
        }
        if actions.take_over {
            remote.acquire(session, true);
        }
        if actions.live {
            self.top = None;
        }
        if let Some(size) = actions.fit {
            remote.resize(session, size.then(|| fit_size(&frame)));
        }

        // The lease as it is now (a tap may have asked for it), renewed while the user is at the terminal, and while
        // the size it asked for is in force: the click on "Fit to this screen" takes the keyboard off the terminal,
        // and the host gives its own size back when the lease ends.
        let lease = remote.lease(session);
        self.held = lease.yours;
        self.sized = lease.yours && lease.resized;
        if lease.yours && (self.focused || self.sized) && now_ms - self.active_ms >= RENEW_MS {
            remote.renew(session);
            self.active_ms = now_ms;
        }

        let frame = self.frame(ui, body, session, remote, p).unwrap_or(frame);
        self.refit(&frame, session, remote);
        self.paint(ui, &frame, session, remote, p, lease.yours);

        let writable = live && may_type && (lease.yours || lease.holder.is_none());
        let copy = self.selection.and_then(|(a, b)| {
            let text = self.selection_text(a, b);
            let line_y = |line: u64| frame.origin.y + line.saturating_sub(frame.top) as f32 * frame.cell.y;
            let above = line_y(a.line.min(b.line)).max(frame.rect.top());
            // Under the last line, and under its handles' knobs where it has them.
            let below = line_y(a.line.max(b.line) + 1) + if self.handles { 2.0 * KNOB } else { 0.0 };
            (!text.is_empty()).then(|| (text, pos2(frame.rect.center().x, above), below.min(frame.rect.bottom())))
        });
        // The overlay keeps the focus only while typing goes somewhere: this client holds the lease or is
        // asking for it. Otherwise (lost, refused, read only) it loses it, and typing again is a new tap.
        let typing = lease.yours || lease.acquiring;
        self.overlay = Overlay {
            rect: Some(frame.rect),
            covered: Vec::new(),
            writable,
            ctrl: self.ctrl,
            alt: self.alt,
            copy,
            blur: lost || std::mem::take(&mut self.blur_next) || (self.focused && !(writable && typing)),
            focus: std::mem::take(&mut self.focus_next) && writable,
            editor: remote.editors().iter().any(|e| e.terminal == session),
        };
        if self.overlay.blur {
            self.focused = false;
        }
    }

    fn header(&self, ui: &mut egui::Ui, lease: &Lease, may_type: bool, live: bool, p: &Palette, actions: &mut Actions) {
        let (text, color) = if !live {
            ("Disconnected: typing is off".to_string(), p.warning)
        } else if !may_type {
            ("View only".to_string(), p.muted)
        } else if lease.yours {
            ("You are typing".to_string(), p.success)
        } else if let Some(holder) = &lease.holder {
            (format!("Typing from {holder}"), p.warning)
        } else {
            // A finger taps, a mouse clicks.
            let act = if self.touch.is_some() { "Tap" } else { "Click" };
            match lease.lost {
                Some(Lost::Desktop) => (format!("The computer took this terminal back. {act} to type."), p.muted),
                _ if lease.acquiring => ("Asking to type…".to_string(), p.muted),
                _ => (format!("{act} the terminal to type"), p.muted),
            }
        };
        ui.label(RichText::new(text).color(color).size(12.5));
        let touch = self.touch;
        let button = |ui: &mut egui::Ui, text: &str| touch::small_button(ui, touch, text);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if self.top.is_some() && button(ui, "↓ Live").on_hover_text("Back to the live screen").clicked() {
                actions.live = true;
            }
            if !live || !may_type {
                return;
            }
            if lease.yours {
                if lease.resized {
                    if button(ui, "Computer's size").clicked() {
                        actions.fit = Some(false);
                    }
                } else if self.fit.is_none()
                    && button(ui, "Fit to this screen")
                    .on_hover_text("Resize the terminal to this screen; the computer shows it, and gets its size back when it types")
                    .clicked()
                {
                    actions.fit = Some(true);
                }
            } else if lease.holder.is_some() && button(ui, "Take over").clicked() {
                actions.take_over = true;
            }
        });
    }

    /// The cell size at the font in use, and which lines show where.
    fn frame(
        &mut self,
        ui: &egui::Ui,
        body: Rect,
        session: &str,
        remote: &RemoteBackend,
        p: &Palette,
    ) -> Option<Frame> {
        let (cols, rows, first, cursor_row, oldest) = remote
            .with_screen(session, |s| s.map(|(s, _)| (s.cols, s.rows, s.first_line, s.cursor.row, s.oldest_line())))?;
        let (pad, fit_min) = match self.touch {
            Some(touch) => (vec2(touch.gutter, 8.0), PHONE_FIT_MIN),
            None => (vec2(PAD, PAD), FIT_MIN),
        };
        let ratio = ui.fonts_mut(|f| f.glyph_width(&p.mono(100.0), 'M')) / 100.0;
        let width_fit = (body.width() - 2.0 * pad.x) / (f32::from(cols) * ratio.max(0.01));
        let size = self.font.or(self.fit).unwrap_or(width_fit.clamp(fit_min, FIT_MAX)).clamp(MIN_FONT, MAX_FONT);
        let font = p.mono(size);
        let cell = ui.fonts_mut(|f| vec2(f.glyph_width(&font, 'M'), f.row_height(&font).ceil()));
        let fit_rows = (((body.height() - 2.0 * pad.y) / cell.y).floor().max(1.0)) as u16;
        let shown = fit_rows.min(rows);
        // The lowest window that still shows the cursor, with a line under it where there is one.
        let below = (u64::from(cursor_row) + 2).saturating_sub(u64::from(shown));
        let live_top = first + below.min(u64::from(rows - shown));
        let top = self.top.map_or(live_top, |top| top.clamp(oldest, live_top));
        if self.top.is_some_and(|t| t >= live_top) {
            self.top = None;
        }
        let width = f32::from(cols) * cell.x;
        self.pan_x = self.pan_x.clamp(0.0, (width - (body.width() - 2.0 * pad.x)).max(0.0));
        let origin = body.min + vec2(pad.x - self.pan_x, pad.y);
        Some(Frame { rect: body, pad, origin, size, cell, first, top, rows: shown, cols, live_top, oldest })
    }

    /// Carry out one input. `lost`: the lease was lost since the last frame, so nothing typed in this one
    /// counts, and a focus from before the overlay was told to give it up does not ask for the lease.
    #[allow(clippy::too_many_arguments)]
    fn input(
        &mut self,
        input: TermInput,
        frame: &Frame,
        modes: &Modes,
        lease: &Lease,
        lost: bool,
        remote: &RemoteBackend,
        session: &str,
        typed: &mut String,
        now_ms: f64,
    ) {
        // Typing counts only while the overlay has the focus (never in the frame the lease was lost, see
        // `draw`). A key bar's sticky modifier is typing too.
        let typing = self.focused;
        match input {
            TermInput::Text(_) | TermInput::Preedit(_) | TermInput::Key { .. } | TermInput::Paste(_) if !typing => {}
            TermInput::Text(text) => {
                typed.push_str(&encode::text(&text, self.ctrl, self.alt));
                (self.ctrl, self.alt) = (false, false);
            }
            TermInput::Preedit(text) => self.preedit = text,
            TermInput::Key { name, .. } if name == "Ctrl" => self.ctrl = !self.ctrl,
            TermInput::Key { name, .. } if name == "Alt" => self.alt = !self.alt,
            TermInput::Key { name, mods } => {
                if let Some(text) = key_text(&name, mods, (self.ctrl, self.alt), modes) {
                    typed.push_str(&text);
                }
                (self.ctrl, self.alt) = (false, false);
            }
            TermInput::Paste(text) => typed.push_str(&encode::paste(&text, modes)),
            TermInput::Focus(true) if lost => {}
            TermInput::Focus(true) => {
                self.focused = true;
                self.active_ms = now_ms;
                // The overlay gaining the focus is the only thing that asks for the lease.
                if !lease.yours && lease.holder.is_none() {
                    remote.acquire(session, false);
                }
            }
            TermInput::Focus(false) => {
                self.focused = false;
                self.preedit.clear();
            }
            TermInput::Pointer(pointer) => {
                // A finger on a selection's handle drags that end; nothing else sees that pointer.
                if let Some(drag) = self.dragging.filter(|d| d.id == pointer.id) {
                    match pointer.phase {
                        Phase::Move | Phase::Up => self.drag_handle(drag, pointer.pos, frame),
                        Phase::Down | Phase::Cancel => {}
                    }
                    if matches!(pointer.phase, Phase::Up | Phase::Cancel) {
                        self.dragging = None;
                    }
                    return;
                }
                if pointer.phase == Phase::Down
                    && pointer.kind != Kind::Mouse
                    && let Some(drag) = self.handle_at(&pointer, frame)
                {
                    self.dragging = Some(drag);
                    return;
                }
                for gesture in self.gestures.event(&pointer) {
                    self.gesture(gesture, frame, modes, lease, typed);
                }
            }
            TermInput::Wheel { delta, at, zoom } => {
                if zoom {
                    self.zoom((-delta.y / 200.0).exp(), at, frame);
                } else {
                    self.scroll(-delta.y, at, frame, modes, lease, typed);
                }
            }
            TermInput::Copied => self.selection = None,
        }
    }

    fn gesture(&mut self, gesture: Gesture, frame: &Frame, modes: &Modes, lease: &Lease, typed: &mut String) {
        match gesture {
            Gesture::Tap(at) => {
                if self.selection.take().is_some() {
                    return;
                }
                if lease.yours && encode::reports_mouse(modes) {
                    let cell = frame.cell_at(at);
                    if let Some(row) = frame.screen_row(cell) {
                        for pressed in [true, false] {
                            if let Some(report) =
                                encode::mouse(encode::Button::Left, pressed, cell.col, row, Modifiers::NONE, modes)
                            {
                                typed.push_str(&report);
                            }
                        }
                    }
                }
            }
            Gesture::Drag(delta) => {
                self.pan_x -= delta.x;
                self.scroll(delta.y, frame.rect.center(), frame, modes, lease, typed);
            }
            Gesture::Pinch { factor, center } => self.zoom(factor, center, frame),
            Gesture::Select { from, to, word: false, .. } => {
                self.selection = Some((frame.cell_at(from), frame.cell_at(to)));
                self.handles = false;
            }
            // A long press: the word under the finger, then out to wherever the finger moves before it lifts.
            Gesture::Select { from, to, word: true, .. } => {
                let (start, end) = self.word_at(frame.cell_at(from));
                let to = frame.cell_at(to);
                self.selection = Some(if to < start {
                    (end, to)
                } else if to > end {
                    (start, to)
                } else {
                    (start, end)
                });
                self.handles = true;
            }
        }
    }

    /// The word around `cell` on the screen as last drawn, or the cell alone where there is none.
    fn word_at(&self, cell: Cell) -> (Cell, Cell) {
        let Some(line) = cell.line.checked_sub(self.view.first_line).and_then(|row| self.view.lines.get(row as usize))
        else {
            return (cell, cell);
        };
        let word = |col: usize| line.cells.get(col).is_some_and(|c| word_char(c.c));
        let mut start = usize::from(cell.col);
        if !word(start) {
            return (cell, cell);
        }
        let mut end = start;
        while start > 0 && word(start - 1) {
            start -= 1;
        }
        while word(end + 1) {
            end += 1;
        }
        let at = |col: usize| Cell { line: cell.line, col: col as u16 };
        (at(start), at(end))
    }

    /// The selection's ends in order, and where their handles' knobs are: under the first cell's left edge
    /// and the last cell's right edge.
    fn knobs(&self, frame: &Frame) -> Option<[(Cell, Pos2); 2]> {
        let (a, b) = self.selection.filter(|_| self.handles)?;
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        let bottom = |cell: Cell| frame.origin.y + (cell.line as f32 - frame.top as f32 + 1.0) * frame.cell.y;
        let x = |col: u16| frame.origin.x + f32::from(col) * frame.cell.x;
        Some([
            (start, pos2(x(start.col), bottom(start) + KNOB)),
            (end, pos2(x(end.col.saturating_add(1)), bottom(end) + KNOB)),
        ])
    }

    /// The handle a finger going down at `pointer` takes, if it is on one.
    fn handle_at(&self, pointer: &Pointer, frame: &Frame) -> Option<HandleDrag> {
        let [start, end] = self.knobs(frame)?;
        let reach = |knob: Pos2| knob.distance(pointer.pos);
        let ((cell, knob), anchor) = if reach(end.1) <= reach(start.1) { (end, start.0) } else { (start, end.0) };
        if reach(knob) > KNOB_REACH {
            return None;
        }
        let centre = frame.origin
            + vec2(
                (f32::from(cell.col) + 0.5) * frame.cell.x,
                (cell.line as f32 - frame.top as f32 + 0.5) * frame.cell.y,
            );
        Some(HandleDrag { id: pointer.id, anchor, grab: centre - pointer.pos })
    }

    /// Move the dragged end of the selection to the cell under the finger, as far as it held the handle.
    fn drag_handle(&mut self, drag: HandleDrag, at: Pos2, frame: &Frame) {
        if self.selection.is_some() {
            self.selection = Some((drag.anchor, frame.cell_at(at + drag.grab)));
        }
    }

    /// The font size times `factor`, keeping the column under `at` where it is.
    fn zoom(&mut self, factor: f32, at: Pos2, frame: &Frame) {
        let current = frame.size;
        let next = (current * factor).clamp(MIN_FONT, MAX_FONT);
        let scale = next / current;
        let x = at.x - frame.rect.left() - frame.pad.x + self.pan_x;
        self.pan_x = x * scale - (at.x - frame.rect.left() - frame.pad.x);
        self.font = Some(next);
    }

    /// Scroll by `dy` points (positive: toward older lines). A program in the alternate screen that
    /// asked for it gets arrows, and one that takes mouse reports wheel reports, as on the desktop;
    /// both only while this client types. Otherwise this client's view moves through history.
    fn scroll(&mut self, dy: f32, at: Pos2, frame: &Frame, modes: &Modes, lease: &Lease, typed: &mut String) {
        self.scroll_rest += dy / frame.cell.y;
        let lines = self.scroll_rest.trunc() as i64;
        self.scroll_rest -= lines as f32;
        if lines == 0 {
            return;
        }
        let count = lines.unsigned_abs() as usize;
        if lease.yours && encode::reports_mouse(modes) && modes.sgr_mouse {
            let cell = frame.cell_at(at);
            let row = frame.screen_row(cell).unwrap_or(0);
            let button = if lines > 0 { encode::Button::WheelUp } else { encode::Button::WheelDown };
            if let Some(report) = encode::mouse(button, true, cell.col, row, Modifiers::NONE, modes) {
                typed.push_str(&report.repeat(count));
            }
            return;
        }
        if lease.yours && modes.alt_screen && modes.alternate_scroll {
            typed.push_str(&(if lines > 0 { "\x1bOA" } else { "\x1bOB" }).repeat(count));
            return;
        }
        let top = frame.top as i64 - lines;
        let top = top.clamp(frame.oldest as i64, frame.live_top as i64) as u64;
        self.top = (top < frame.live_top).then_some(top);
    }

    fn paint(
        &mut self,
        ui: &mut egui::Ui,
        frame: &Frame,
        session: &str,
        remote: &RemoteBackend,
        p: &Palette,
        yours: bool,
    ) {
        self.painted = Some(*frame);
        remote.fill_screen(session, frame.top, frame.rows, &mut self.view);
        let metrics = Metrics { font: p.mono(frame.size), cell: frame.cell };
        let inner = Rect::from_min_size(
            frame.origin,
            vec2(f32::from(frame.cols) * frame.cell.x, f32::from(frame.rows) * frame.cell.y),
        );
        let selection = self.selection.map(|(a, b)| {
            let (a, b) = if a <= b { (a, b) } else { (b, a) };
            let row = |c: Cell| c.line as i64 - frame.top as i64;
            Selection {
                start: Point { row: row(a).clamp(i32::MIN as i64, i32::MAX as i64) as i32, col: a.col },
                end: Point { row: row(b).clamp(i32::MIN as i64, i32::MAX as i64) as i32, col: b.col },
                block: false,
            }
        });
        // The screen's own background, the desktop's, fills the pane below a grid shorter than it.
        let background = self
            .view
            .palette
            .get(usize::from(grid::BACKGROUND))
            .map(|grid::Rgb([r, g, b])| egui::Color32::from_rgb(*r, *g, *b));
        let theme = painter::Theme { background: background.unwrap_or(p.terminal), ..p.terminal_theme() };
        painter::paint(
            ui.painter(),
            (frame.rect, inner),
            &self.view,
            selection.as_ref(),
            &metrics,
            &theme,
            self.focused && yours,
        );
        let stale = remote.with_screen(session, |s| s.is_some_and(|(_, stale)| stale));
        if stale || !remote.live() {
            ui.painter().rect_filled(frame.rect, 0.0, p.terminal.gamma_multiply(0.55));
            ui.painter().text(
                frame.rect.center_top() + vec2(0.0, 18.0),
                Align2::CENTER_CENTER,
                "Disconnected",
                p.ui(13.0),
                p.warning,
            );
        }
        // The composition, over the cursor's cell, until it is committed.
        if !self.preedit.is_empty() && self.view.cursor.shape != atomics_core::screen::CursorShape::Hidden {
            let at = frame.origin
                + vec2(f32::from(self.view.cursor.col) * frame.cell.x, f32::from(self.view.cursor.row) * frame.cell.y);
            let galley = ui.painter().layout_no_wrap(self.preedit.clone(), metrics.font.clone(), p.foreground);
            let back = Rect::from_min_size(at, vec2(galley.size().x.max(frame.cell.x), frame.cell.y));
            ui.painter().rect_filled(back, 2.0, p.raised);
            ui.painter().galley(at, galley, p.foreground);
            ui.painter().line_segment([back.left_bottom(), back.right_bottom()], egui::Stroke::new(1.5, p.accent));
        }
        // A touch selection's handles: a stem along each end's line and a knob under it to drag.
        if let Some(knobs) = self.knobs(frame) {
            let painter = ui.painter().with_clip_rect(frame.rect);
            for (_, knob) in knobs {
                let stem = [knob - vec2(0.0, KNOB + frame.cell.y), knob - vec2(0.0, KNOB)];
                painter.line_segment(stem, Stroke::new(2.0, p.accent));
                painter.circle_filled(knob, KNOB, p.accent);
            }
        }
        ui.allocate_rect(frame.rect, Sense::hover());
    }

    /// The selected text, line by line; a line that wraps onto the next joins it.
    fn selection_text(&self, a: Cell, b: Cell) -> String {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let mut out = String::new();
        let top = self.view.first_line;
        for (row, line) in self.view.lines.iter().enumerate() {
            let id = top + row as u64;
            if id < a.line || id > b.line {
                continue;
            }
            let from = if id == a.line { usize::from(a.col) } else { 0 };
            let to = if id == b.line { usize::from(b.col) + 1 } else { line.cells.len() };
            let mut text = String::new();
            for cell in line.cells.iter().take(to.min(line.cells.len())).skip(from).filter(|c| c.width > 0) {
                text.push(if cell.c == '\0' { ' ' } else { cell.c });
                text.extend(cell.combining.iter().flat_map(|c| c.iter()));
            }
            if id != b.line && line.wrapped {
                out.push_str(&text);
            } else {
                out.push_str(text.trim_end());
                if id != b.line {
                    out.push('\n');
                }
            }
        }
        out
    }
}

/// A terminal not received yet, or not while offline.
fn waiting(ui: &egui::Ui, rect: Rect, live: bool, p: &Palette) {
    ui.painter().rect_filled(rect, 0.0, p.terminal);
    let text = if live { "Waiting for the terminal…" } else { "Not connected" };
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, p.ui(13.0), p.muted);
}

/// The note for typing refused as too much ([`Refused::TooMuch`]), naming the limit that refused it.
fn too_much(limit: usize) -> String {
    let size = if limit >= 1 << 20 { format!("{} MiB", limit >> 20) } else { format!("{} KiB", limit >> 10) };
    if limit == MAX_WAITING_BYTES {
        format!(
            "Too much to type before this browser may type here, so none of it was typed. Up to {size} waits \
             until then."
        )
    } else {
        format!(
            "Too much to type at once, so none of it was typed. A paste takes up to {size}, and less while \
             earlier input still waits."
        )
    }
}

/// The grid that fits the pane at the font in use.
fn fit_size(frame: &Frame) -> GridSize {
    let cols = ((frame.rect.width() - 2.0 * frame.pad.x) / frame.cell.x).floor().max(2.0) as u16;
    let rows = ((frame.rect.height() - 2.0 * frame.pad.y) / frame.cell.y).floor().max(1.0) as u16;
    let pixels = |v: f32| Some((v.round() as u16).clamp(1, MAX_CELL_PIXELS));
    GridSize { cols, rows, cell_width: pixels(frame.cell.x), cell_height: pixels(frame.cell.y) }
}

/// The parts of the overlay at `rect`, over a terminal in `layer`, that egui would not give the terminal the
/// pointer over: a floating window drawn above it this frame, with the margin its resize handles take outside
/// it, and all of it while a menu or popup is open, since a click anywhere then closes that first. A tooltip
/// takes no pointer.
fn covered(ctx: &egui::Context, layer: LayerId, rect: Rect) -> Vec<Rect> {
    let layers: Vec<LayerId> = ctx.memory(|m| m.layer_ids().collect());
    let own = layers.iter().position(|l| *l == layer);
    let grab = ctx.global_style().interaction.resize_grab_radius_side;
    let mut covered = Vec::new();
    for (at, above) in layers.iter().enumerate() {
        if above.order < layer.order || (above.order == layer.order && own.is_none_or(|own| at <= own)) {
            continue;
        }
        // Drawn this frame: egui goes on finding a layer for one frame after it stops being shown.
        let drawn = ctx.graphics(|graphics| graphics.get(*above).is_some_and(|shapes| !shapes.is_empty()));
        let Some(area) = egui::AreaState::load(ctx, above.id).filter(|area| area.interactable && drawn) else {
            continue;
        };
        match above.order {
            Order::Middle => covered.push(area.rect().expand(grab).intersect(rect)),
            Order::Foreground => return vec![rect],
            _ => {}
        }
    }
    covered.retain(|part| part.is_positive());
    covered
}

/// `part` without `cover`: what is left of a rectangle, as up to four rectangles.
fn without(part: Rect, cover: Rect) -> Vec<Rect> {
    let cover = part.intersect(cover);
    if !cover.is_positive() {
        return vec![part];
    }
    let row = cover.y_range();
    [
        part.with_max_y(cover.top()),
        part.with_min_y(cover.bottom()),
        Rect::from_x_y_ranges(part.left()..=cover.left(), row),
        Rect::from_x_y_ranges(cover.right()..=part.right(), row),
    ]
    .into_iter()
    .filter(|left| left.is_positive())
    .collect()
}

/// The overlay at `rect` without `covered`, as a CSS `clip-path` in pixels from its corner at egui's `zoom`:
/// the browser sends a clipped part's pointer to the canvas under it. Empty when nothing covers it.
pub fn clip_path(rect: Rect, covered: &[Rect], zoom: f32) -> String {
    if covered.is_empty() {
        return String::new();
    }
    let mut open = vec![rect];
    for cover in covered {
        open = open.into_iter().flat_map(|part| without(part, *cover)).collect();
    }
    if open.is_empty() {
        return "inset(50%)".into();
    }
    let corner = rect.min.to_vec2();
    let parts: String = open
        .iter()
        .map(|part| {
            let (min, max) = ((part.min - corner) * zoom, (part.max - corner) * zoom);
            format!("M{} {}H{}V{}H{}Z", min.x, min.y, max.x, max.y, min.x)
        })
        .collect();
    format!("path(\"{parts}\")")
}

/// The browser's terminals, for the shared views (`TerminalSurface`). The one the page's input goes to
/// (`target`) is drawn by `pane`, with the overlay; any other as it is, by a pane of its own, and a click
/// on it picks it ([`Terminals::picked`]).
pub struct Terminals<'a> {
    pub pane: &'a mut TermPane,
    pub others: &'a mut HashMap<String, TermPane>,
    pub remote: &'a RemoteBackend,
    pub target: Option<String>,
    pub now_ms: f64,
    /// A native client: it gives the terminals their panes' sizes where the host has no desktop of its own
    /// ([`RemoteBackend::fits`]).
    pub fit: bool,
    /// The terminal a click picked while this drew, by session id.
    pub picked: Option<String>,
}

impl TerminalSurface for Terminals<'_> {
    fn show(
        &mut self,
        ui: &mut egui::Ui,
        _index: usize,
        session: &str,
        rect: Rect,
        id: egui::Id,
        metrics: &Metrics,
        palette: &Palette,
    ) -> egui::Response {
        let fit = (self.fit && self.remote.fits()).then_some(metrics.font.size);
        if self.target.as_deref() == Some(session) {
            self.pane.fit = fit;
            self.pane.draw(ui, rect, session, self.remote, palette, self.now_ms);
            // The overlay lies over it and takes its pointer.
            return ui.interact(rect, id, Sense::hover());
        }
        let pane = self.others.entry(session.to_string()).or_default();
        pane.touch = self.pane.touch;
        pane.fit = fit;
        pane.show(ui, rect, session, self.remote, palette);
        let response = ui.interact(rect, id, Sense::click());
        if response.clicked() {
            self.picked = Some(session.to_string());
        }
        response
    }
}

#[derive(Default)]
struct Actions {
    take_over: bool,
    live: bool,
    /// Fit to this screen (`true`) or back to the computer's size.
    fit: Option<bool>,
}

#[cfg(test)]
mod tests;
