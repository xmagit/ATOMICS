//! Native terminal client: keyboard/pointer input and selection. Painting is the shared painter's
//! (`atomics_ui::terminal`), over a copy of the cells this view displays.

#[cfg(test)]
mod frame_bench;
#[cfg(test)]
mod legacy_paint;
#[cfg(test)]
mod paint_tests;
mod runtime;
pub(crate) mod screen;
#[cfg(test)]
pub(crate) use runtime::Detached;
pub use runtime::Terminal;

use crate::theme::{Palette, Theme, is_app_chord};
use alacritty_terminal::event::WindowSize;
use alacritty_terminal::grid::{Dimensions, Row, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::vte::ansi::Rgb;
use atomics_core::screen::{CursorShape, Screen};
use atomics_core::theme::{parse_color, xterm_color};
use atomics_ui::terminal as painter;
pub use atomics_ui::terminal::Metrics;
use atomics_ui::terminal::encode;
use egui::{Key, Modifiers, Pos2, Rect, Sense, vec2};

pub struct TerminalView {
    terminal: Option<std::sync::Weak<runtime::Shared>>,
    grid: (usize, usize),
    scroll_remainder: f32,
    selection: Option<Selection>,
    selection_revision: u64,
    selection_rows: Option<SelectedRows>,
    /// A copy consumes the whole key press, including repeats, until C or Ctrl is released.
    copying: bool,
    /// Whether Ctrl was down when the last frame ended, which this frame's events start from.
    ctrl: bool,
    /// What has been typed since the last Enter.
    line: String,
    /// The cells last painted; kept so a frame reuses its buffers.
    screen: Screen,
    /// What `screen`'s cells were copied at. Output and every resize (the desktop's, a lease
    /// holder's, a take-back) move the terminal's revision, so while it, the scroll position, the
    /// history, the emulator's size and the view's rows stay put the cells are still current.
    copied: Option<Copied>,
    /// Where the last frame drew "Take back", while a remote client held the lease.
    take_back: Option<Rect>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Copied {
    revision: u64,
    offset: usize,
    history: usize,
    /// The emulator's columns and rows, which a remote lease holder may set apart from the pane.
    size: (usize, usize),
    rows: usize,
}

struct SelectedRows {
    start: Line,
    rows: Vec<Row<Cell>>,
    size: (usize, usize),
}

impl Default for TerminalView {
    fn default() -> Self {
        Self {
            terminal: None,
            grid: (80, 24),
            scroll_remainder: 0.0,
            selection: None,
            selection_revision: 0,
            selection_rows: None,
            copying: false,
            ctrl: false,
            line: String::new(),
            screen: Screen::blank(0, 0),
            copied: None,
            take_back: None,
        }
    }
}

impl TerminalView {
    /// Draws the terminal into `rect` and, when focused, feeds it keyboard and mouse input.
    pub fn show(
        &mut self,
        terminal: &Terminal,
        ui: &mut egui::Ui,
        rect: Rect,
        id: egui::Id,
        metrics: &Metrics,
        palette: &Palette,
    ) -> egui::Response {
        let instance = std::sync::Arc::downgrade(&terminal.shared);
        if !self.terminal.as_ref().is_some_and(|previous| previous.ptr_eq(&instance)) {
            self.terminal = Some(instance);
            self.grid = (0, 0);
            self.selection = None;
            self.selection_rows = None;
            self.copying = false;
            self.scroll_remainder = 0.0;
            self.line.clear();
            self.copied = None;
        }
        let response = ui.interact(rect, id, Sense::click_and_drag());
        if response.clicked() || response.drag_started() {
            response.request_focus();
        }
        let focused = response.has_focus();
        terminal.shown(focused);
        if focused {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    id,
                    egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
                )
            });
        }

        let inner = rect.shrink2(vec2(6.0, 4.0));
        self.resize(terminal, inner, metrics);
        self.handle_mouse(terminal, ui, &response, inner, metrics);
        if focused {
            self.handle_keys(terminal, ui);
        } else {
            self.ctrl = ui.input(|i| i.modifiers.ctrl);
            self.copying &= self.ctrl;
        }
        self.paint(terminal, ui, (rect, inner), metrics, palette, focused);
        self.take_back = None;
        if let Some((holder, sized)) = terminal.remote_holder() {
            self.holder_pill(terminal, ui, rect, id, (&holder, sized), palette);
        }
        response
    }

    /// "Typing from <device>" over the pane's top-right corner while a remote client holds the
    /// lease, with "Take back", which ends its lease and with it any size it set, as typing here
    /// does. It floats over the cells, so the pane and the grid it asks for keep their size.
    fn holder_pill(
        &mut self,
        terminal: &Terminal,
        ui: &egui::Ui,
        rect: Rect,
        id: egui::Id,
        (holder, sized): (&str, bool),
        palette: &Palette,
    ) {
        let text =
            if sized { format!("Typing from {holder} · sized for it") } else { format!("Typing from {holder}") };
        let font = palette.ui(12.0);
        let painter = ui.painter().with_clip_rect(rect);
        let label = painter.layout_no_wrap(text, font.clone(), palette.foreground);
        let action = painter.layout_no_wrap("Take back".into(), font, palette.on_accent);
        let pad = vec2(8.0, 4.0);
        let height = label.size().y.max(action.size().y) + 2.0 * pad.y;
        let button = vec2(action.size().x + 2.0 * pad.x, height - 4.0);
        let width = pad.x + label.size().x + pad.x + button.x + 2.0;
        let pill = Rect::from_min_size(egui::pos2(rect.right() - width - 6.0, rect.top() + 6.0), vec2(width, height));
        painter.rect_filled(pill, 6.0, palette.raised);
        painter.rect_stroke(pill, 6.0, egui::Stroke::new(1.0, palette.accent), egui::StrokeKind::Inside);
        painter.galley(
            egui::pos2(pill.left() + pad.x, pill.center().y - label.size().y / 2.0),
            label,
            palette.foreground,
        );
        let at = Rect::from_min_size(egui::pos2(pill.right() - button.x - 2.0, pill.top() + 2.0), button);
        let response = ui
            .interact(at, id.with("take back"), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text("End the remote device's typing here; the terminal returns to this pane's size");
        let fill = if response.hovered() { palette.accent.gamma_multiply(0.85) } else { palette.accent };
        painter.rect_filled(at, 4.0, fill);
        painter.galley(at.center() - action.size() / 2.0, action, palette.on_accent);
        self.take_back = Some(at);
        if response.clicked() {
            terminal.take_back();
        }
    }

    fn resize(&mut self, terminal: &Terminal, rect: Rect, metrics: &Metrics) {
        let cols = ((rect.width() / metrics.cell.x).floor() as usize).max(2);
        let rows = ((rect.height() / metrics.cell.y).floor() as usize).max(1);
        if (cols, rows) == self.grid {
            return;
        }
        self.grid = (cols, rows);
        self.selection = None;
        self.selection_rows = None;
        let size = WindowSize {
            num_lines: rows as u16,
            num_cols: cols as u16,
            cell_width: metrics.cell.x as u16,
            cell_height: metrics.cell.y as u16,
        };
        terminal.resize(size);
    }

    // Client anchors are grid coordinates. Unrelated output leaves them alone; a rewrite,
    // scroll or resize that changes their rows invalidates them. The caller holds the emulator lock.
    fn sync_selection(&mut self, terminal: &Terminal, term: &alacritty_terminal::Term<runtime::Listener>) {
        let revision = terminal.revision();
        if self.selection_revision != revision {
            if let Some(SelectedRows { start, rows, size }) = &self.selection_rows {
                let grid = term.grid();
                if *size != (grid.columns(), grid.screen_lines())
                    || *start < grid.topmost_line()
                    || rows.iter().enumerate().any(|(n, row)| grid[*start + n] != *row)
                {
                    self.selection = None;
                    self.selection_rows = None;
                }
            }
            self.selection_revision = revision;
        }
    }

    fn remember_selection(&mut self, term: &alacritty_terminal::Term<runtime::Listener>) {
        self.selection_rows = self.selection.as_ref().and_then(|s| s.to_range(term)).map(|range| {
            let grid = term.grid();
            let rows = (range.start.line.0..=range.end.line.0).map(|line| grid[Line(line)].clone()).collect();
            SelectedRows { start: range.start.line, rows, size: (grid.columns(), grid.screen_lines()) }
        });
    }

    fn point_at(&self, pos: Pos2, rect: Rect, metrics: &Metrics, offset: usize) -> (Point, Side) {
        let x = ((pos.x - rect.left()) / metrics.cell.x).max(0.0);
        let col = (x.floor() as usize).min(self.grid.0 - 1);
        let row = (((pos.y - rect.top()) / metrics.cell.y).max(0.0).floor() as usize).min(self.grid.1 - 1);
        let side = if x.fract() < 0.5 { Side::Left } else { Side::Right };
        (Point::new(Line(row as i32 - offset as i32), Column(col)), side)
    }

    fn handle_mouse(
        &mut self,
        terminal: &Terminal,
        ui: &egui::Ui,
        response: &egui::Response,
        rect: Rect,
        metrics: &Metrics,
    ) {
        let mut term = terminal.term.lock();
        self.sync_selection(terminal, &term);
        let offset = term.grid().display_offset();

        if let Some(pos) = response.interact_pointer_pos() {
            let (point, side) = self.point_at(pos, rect, metrics, offset);
            if response.drag_started() {
                let origin = ui.input(|i| i.pointer.press_origin()).unwrap_or(pos);
                let (point, side) = self.point_at(origin, rect, metrics, offset);
                self.selection = Some(Selection::new(SelectionType::Simple, point, side));
            } else if response.dragged()
                && let Some(selection) = self.selection.as_mut()
            {
                selection.update(point, side);
            }
        }
        if response.clicked() {
            self.selection = None;
        }
        if response.drag_started() || response.dragged() || response.clicked() {
            self.remember_selection(&term);
        }

        if !response.hovered() {
            return;
        }
        let delta = ui.input(|i| i.smooth_scroll_delta.y);
        if delta == 0.0 {
            return;
        }
        self.scroll_remainder += delta / metrics.cell.y;
        let lines = self.scroll_remainder.trunc() as i32;
        self.scroll_remainder -= lines as f32;
        if lines == 0 {
            return;
        }
        let mode = *term.mode();
        if mode.intersects(TermMode::MOUSE_MODE) && mode.contains(TermMode::SGR_MOUSE) {
            let pos = ui.input(|i| i.pointer.hover_pos()).unwrap_or(rect.min);
            let (point, _) = self.point_at(pos, rect, metrics, offset);
            let button = if lines > 0 { encode::Button::WheelUp } else { encode::Button::WheelDown };
            let row = (point.line.0 + offset as i32).max(0) as u16;
            let report = encode::mouse(button, true, point.column.0 as u16, row, Modifiers::NONE, &screen::modes(mode));
            drop(term);
            if let Some(report) = report {
                terminal.write(report.repeat(lines.unsigned_abs() as usize).into_bytes());
            }
        } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
            let key = if lines > 0 { "\x1bOA" } else { "\x1bOB" };
            drop(term);
            terminal.write(key.repeat(lines.unsigned_abs() as usize).into_bytes());
        } else {
            term.scroll_display(Scroll::Delta(lines));
        }
    }

    fn handle_keys(&mut self, terminal: &Terminal, ui: &egui::Ui) {
        let events = ui.input(|i| i.events.clone());
        // A frame can hold a whole chord, and egui's modifiers say only how it ended: Ctrl as each event saw it.
        let mut ctrl = self.ctrl;
        let modes = screen::modes(*terminal.term.lock().mode());
        let mut out: Vec<u8> = Vec::new();
        for event in events {
            match event {
                egui::Event::ModifiersChanged(modifiers) => {
                    ctrl = modifiers.ctrl;
                    if !ctrl {
                        self.copying = false;
                    }
                }
                egui::Event::Text(text) => {
                    let mods = ui.input(|i| i.modifiers);
                    if !is_app_chord(&mods) && !mods.ctrl {
                        out.extend_from_slice(text.as_bytes());
                        self.line.push_str(&text);
                        if !text.is_empty() {
                            terminal.record_input(std::time::Instant::now(), false);
                        }
                    }
                }
                egui::Event::Paste(text) => {
                    out.extend(encode::paste(&text, &modes).into_bytes());
                    self.line.push_str(&text);
                    if !text.is_empty() {
                        terminal.record_input(std::time::Instant::now(), false);
                    }
                }
                egui::Event::Copy => {
                    let mut term = terminal.term.lock();
                    self.sync_selection(terminal, &term);
                    // alacritty's copy helper reads its selection field. Install the
                    // client's selection only for this locked query, then restore it.
                    let previous = std::mem::replace(&mut term.selection, self.selection.clone());
                    let text = term.selection_to_string();
                    term.selection = previous;
                    if let Some(text) = text.filter(|text| !text.is_empty()) {
                        ui.ctx().copy_text(text);
                        self.selection = None;
                        self.selection_rows = None;
                        self.copying = true;
                    } else if !cfg!(target_os = "macos") && ctrl && !self.copying {
                        out.push(0x03);
                        terminal.record_input(std::time::Instant::now(), false);
                    }
                }
                egui::Event::Cut => {
                    if !cfg!(target_os = "macos") {
                        out.push(0x18);
                        terminal.record_input(std::time::Instant::now(), false);
                    }
                }
                egui::Event::Key { key: Key::C, pressed: false, .. } => self.copying = false,
                egui::Event::Key { key, pressed: true, modifiers, .. } if !is_app_chord(&modifiers) => {
                    if let Some(bytes) = encode::key(key, modifiers, &modes) {
                        out.extend(bytes);
                        terminal.record_input(
                            std::time::Instant::now(),
                            key == Key::Enter && !modifiers.shift && !modifiers.alt,
                        );
                        if let Some(line) = typed_key(&mut self.line, key, modifiers) {
                            *terminal.shared.submitted.lock().unwrap() = Some(line);
                        }
                    }
                }
                _ => {}
            }
        }
        self.ctrl = ui.input(|i| i.modifiers.ctrl);
        if !self.ctrl {
            self.copying = false;
        }
        if !out.is_empty() {
            // Typing on the desktop takes back a remote client's lease (a no-op without one).
            terminal.take_back();
            let mut term = terminal.term.lock();
            term.scroll_display(Scroll::Bottom);
            self.selection = None;
            self.selection_rows = None;
            drop(term);
            terminal.write(out);
        }
    }

    fn paint(
        &mut self,
        terminal: &Terminal,
        ui: &egui::Ui,
        (outer, rect): (Rect, Rect),
        metrics: &Metrics,
        palette: &Palette,
        focused: bool,
    ) {
        // Copy under the emulator lock; lay out and paint after releasing it.
        let selection = {
            let term = terminal.term.lock();
            self.sync_selection(terminal, &term);
            self.copy_view(&term, palette, Some(terminal.revision()))
        };
        painter::paint(
            ui.painter(),
            (outer, rect),
            &self.screen,
            selection.as_ref(),
            metrics,
            &palette.terminal_theme(),
            focused,
        );
    }

    /// Copy the displayed cells into `self.screen`, unless they are unchanged since the copy at
    /// `revision`, and return the selection in their rows. The cursor shows only on this view's
    /// rows, as it always has.
    fn copy_view<T>(
        &mut self,
        term: &alacritty_terminal::Term<T>,
        palette: &Palette,
        revision: Option<u64>,
    ) -> Option<painter::Selection> {
        use alacritty_terminal::grid::Dimensions;
        let grid = term.grid();
        let offset = grid.display_offset();
        let key = revision.map(|revision| Copied {
            revision,
            offset,
            history: grid.history_size(),
            size: (grid.columns(), grid.screen_lines()),
            rows: self.grid.1,
        });
        let theme = slot_colors(palette);
        if key.is_some() && key == self.copied {
            screen::view_palette(term, &theme, &mut self.screen);
        } else {
            screen::view(term, &theme, &mut self.screen);
            if usize::from(self.screen.cursor.row) >= self.grid.1 {
                self.screen.cursor.shape = CursorShape::Hidden;
            }
            self.copied = key;
        }
        let selection = self.selection.as_ref().and_then(|s| s.to_range(term));
        selection.map(|range| view_selection(range, offset as i32))
    }
}

/// An emulator selection in the rows of the displayed screen.
fn view_selection(range: SelectionRange, offset: i32) -> painter::Selection {
    let point = |p: Point| painter::Point { row: p.line.0 + offset, col: p.column.0 as u16 };
    painter::Selection { start: point(range.start), end: point(range.end), block: range.is_block }
}

/// Terminal input for keys that don't arrive as text.
/// Follows the line being typed: Enter hands it back, unless it is empty.
fn typed_key(line: &mut String, key: Key, m: Modifiers) -> Option<String> {
    match key {
        Key::Enter if m.shift || m.alt => line.push('\n'),
        Key::Enter => return Some(std::mem::take(line)).filter(|line| !line.trim().is_empty()),
        Key::Backspace if m.alt || m.ctrl => line.clear(),
        Key::Backspace => {
            line.pop();
        }
        // Escape, Ctrl+C and Ctrl+U throw the line away.
        Key::Escape => line.clear(),
        _ if m.ctrl => line.clear(),
        _ => {}
    }
    None
}

/// The palette's colours for the cell slots the desktop copy fills: the 256 indexed colours,
/// then foreground, background and cursor. They are [`Theme::terminal_colors`] as the painter
/// draws them (`terminals_answer_the_colors_the_painter_draws`).
fn slot_colors(palette: &Palette) -> Vec<Rgb> {
    (0..=258)
        .map(|index| {
            let color = match index {
                256 => palette.foreground,
                257 => palette.terminal,
                258 => palette.cursor,
                i => palette.indexed(i),
            };
            Rgb { r: color.r(), g: color.g(), b: color.b() }
        })
        .collect()
}

impl Theme {
    /// The theme's 256 colors, then foreground, background and cursor: what terminals answer to
    /// OSC 4/10/11/12. The same colors the painter takes from the [`Palette`], read from the tokens.
    pub fn terminal_colors(&self) -> Vec<Rgb> {
        let t = &self.tokens;
        // A token that does not parse is black, as in the palette.
        let color = |value: Option<&String>| value.and_then(|value| parse_color(value)).unwrap_or([0; 3]);
        let ansi = t.ansi.as_deref().unwrap_or_default();
        (0..=258)
            .map(|index| {
                let [r, g, b] = match index {
                    0..16 => color(ansi.get(index)),
                    256 => color(t.foreground.as_ref()),
                    257 => color(t.terminal.as_ref()),
                    258 => color(t.cursor.as_ref()),
                    xterm => xterm_color(xterm).expect("colors 16 to 255 are xterm's"),
                };
                Rgb { r, g, b }
            })
            .collect()
    }
}

/// A shell that prints `ready`, then for every key it reads sends `query` and shows the
/// `reply_len`-byte answer on screen, with `^` for ESC and `!` for BEL.
#[cfg(test)]
pub(crate) fn color_query_args(query: &str, reply_len: usize) -> Vec<String> {
    let script = format!(
        "stty raw -echo; printf 'ready\\r\\n'; while dd bs=1 count=1 >/dev/null 2>&1; do printf '{query}'; \
         r=$(dd bs=1 count={reply_len} 2>/dev/null); printf '%s\\r\\n' \"$r\" | tr '\\033\\007' '^!'; done"
    );
    vec!["-c".into(), script]
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Color32, FontId};
    use std::{collections::HashMap, time::Duration};

    #[test]
    fn chief_input_tracking_follows_keystrokes_pastes_and_enter() {
        let dir = crate::test_support::tempdir().unwrap();
        let terminal = Terminal::spawn(
            crate::host::Wake::default(),
            Vec::new(),
            crate::test_support::program("cat"),
            vec![],
            dir.path(),
            HashMap::new(),
            100,
        )
        .unwrap();
        let ctx = egui::Context::default();
        let mut view = TerminalView::default();
        let ended = Some(std::time::Instant::now() - Duration::from_secs(1));
        let enter = |modifiers| egui::Event::Key {
            key: Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        for (events, empty) in [
            (vec![egui::Event::Text("draft".into())], false),
            (vec![enter(Modifiers::SHIFT)], false),
            (vec![enter(Modifiers::NONE)], true),
            (vec![egui::Event::Paste("pasted\ntext".into())], false),
            (vec![enter(Modifiers::NONE)], true),
            (vec![enter(Modifiers::NONE), egui::Event::Text("next draft".into())], false),
        ] {
            ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| view.handle_keys(&terminal, ui))
                .drop_without_applying_deltas();
            assert_eq!(terminal.input_empty_since(ended), empty);
            assert!(terminal.last_typed().is_some());
        }
    }

    #[test]
    fn chief_input_fallback_ignores_dim_and_colored_suggestions() {
        let dir = crate::test_support::tempdir().unwrap();
        let terminal = Terminal::spawn(
            crate::host::Wake::default(),
            Vec::new(),
            crate::test_support::program("sleep"),
            vec!["30".into()],
            dir.path(),
            HashMap::new(),
            100,
        )
        .unwrap();
        let mut parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
        for (prompt, empty) in [
            ("❯ \x1b[2mrun the full suite once more\x1b[0m", true),
            ("\x1b[32m❯ \x1b[90mrun tests > /tmp/result\x1b[0m", true),
            ("❯ \x1b[38;2;128;128;128mrun tests\x1b[0m", true),
            ("❯ native-looking draft", false),
            ("❯\r\n  second draft line\r\n────", false),
            ("unknown screen", true),
        ] {
            parser.advance(&mut *terminal.term.lock(), format!("\x1b[2J\x1b[H{prompt}").as_bytes());
            assert_eq!(terminal.input_empty_since(None), empty, "{prompt:?}");
        }
        // Once input is known, screen contents never override the recorded submit.
        terminal.record_input(std::time::Instant::now(), true);
        parser.advance(&mut *terminal.term.lock(), b"\x1b[2J\x1b[H> unstyled suggestion");
        assert!(terminal.input_empty_since(None));
    }

    #[test]
    fn screen_reader_defers_terminal_lock_to_worker() {
        let theme = Theme::from_config(&atomics_core::config::ThemeConfig::default());
        let dir = crate::test_support::tempdir().unwrap();
        let terminal = Terminal::spawn(
            crate::host::Wake::default(),
            theme.terminal_colors(),
            crate::test_support::program("cat"),
            vec![],
            dir.path(),
            HashMap::new(),
            100,
        )
        .unwrap();
        let guard = terminal.term.lock();
        let read = terminal.screen_reader(3);
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            tx.send(read()).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
        drop(guard);
        assert!(rx.recv_timeout(Duration::from_secs(2)).unwrap().is_empty());
        worker.join().unwrap();
        terminal.shutdown();
    }

    #[test]
    fn screen_text_is_the_live_screen_while_the_display_is_scrolled() {
        let (terminal, _view, _metrics, _dir) = selected_terminal();
        {
            let mut term = terminal.term.lock();
            let mut parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
            parser.advance(&mut *term, "line\r\n".repeat(200).as_bytes());
            parser.advance(&mut *term, b"\r\nesc to interrupt");
            term.scroll_display(Scroll::Top);
            assert!(term.grid().display_offset() > 0);
        }
        assert!(terminal.screen_text(24).contains("esc to interrupt"));
        terminal.shutdown();
    }

    #[test]
    fn desktop_typing_reaches_the_pty_as_before_and_takes_a_remote_lease_back() {
        use crate::protocol::TerminalControl;
        use crate::protocol::lease::{Client, Policy};
        use alacritty_terminal::grid::Dimensions;
        use atomics_core::lease::{GridSize, LeaseReason};
        use atomics_core::wire::ErrorCode;

        let dir = crate::test_support::tempdir().unwrap();
        let terminal = Terminal::spawn(
            crate::host::Wake::default(),
            Vec::new(),
            crate::test_support::program("cat"),
            vec![],
            dir.path(),
            HashMap::new(),
            100,
        )
        .unwrap();
        let ctx = egui::Context::default();
        let mut view = TerminalView::default();
        let type_text = |view: &mut TerminalView, text: &str| {
            let events = vec![egui::Event::Text(text.into())];
            ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| view.handle_keys(&terminal, ui))
                .drop_without_applying_deltas();
        };
        let columns = || terminal.term.lock().grid().columns();
        // Nobody holds a lease, as always with remote access off: typing is what it was.
        type_text(&mut view, "one");
        wait_for_text(&terminal, "one");

        // A remote client holds the lease and fitted the terminal to its screen.
        let handle = terminal.screen_handle();
        let control: &dyn TerminalControl = &handle;
        let (phone, policy, now) =
            (Client { id: u64::MAX, label: "phone".into() }, Policy::default(), std::time::Instant::now());
        control.acquire(&phone, false, now, &policy).unwrap();
        let token = control.view("s", &phone, &policy).1.lease.unwrap().0;
        control.resize_for(&phone, token, Some(GridSize::new(40, 12)), now, &policy).unwrap();
        assert_eq!(columns(), 40);
        // The desktop's pane changes meanwhile: its size waits for the lease.
        let metrics = Metrics { font: FontId::monospace(12.0), cell: vec2(8.0, 16.0) };
        view.resize(&terminal, Rect::from_min_size(Pos2::ZERO, vec2(800.0, 480.0)), &metrics);
        assert_eq!(columns(), 40);

        // Typing on the desktop goes through as always, and takes the terminal back with its size.
        type_text(&mut view, "two");
        wait_for_text(&terminal, "onetwo");
        let (_, state) = control.view("s", &phone, &policy);
        assert_eq!((state.holder, state.reason), (None, Some(LeaseReason::Desktop)));
        assert_eq!(columns(), 100);
        let late = control.input(&phone, (token, 1), b"late".to_vec(), now, &policy).unwrap_err();
        assert_eq!(late.code, ErrorCode::Fenced);
        std::thread::sleep(Duration::from_millis(50));
        assert!(!terminal.screen_text(24).contains("late"));
        terminal.shutdown();
    }

    #[test]
    fn the_desktop_shows_who_types_and_takes_the_terminal_back_in_one_click() {
        use crate::protocol::TerminalControl;
        use crate::protocol::lease::{Client, Policy};
        use alacritty_terminal::grid::Dimensions;
        use atomics_core::lease::{GridSize, LeaseReason};

        let dir = crate::test_support::tempdir().unwrap();
        let terminal = Terminal::spawn(
            crate::host::Wake::default(),
            Vec::new(),
            crate::test_support::program("cat"),
            vec![],
            dir.path(),
            HashMap::new(),
            100,
        )
        .unwrap();
        let palette = Theme::from_config(&atomics_core::config::ThemeConfig::default()).palette();
        let metrics = Metrics { font: FontId::monospace(12.0), cell: vec2(8.0, 16.0) };
        let rect = Rect::from_min_size(Pos2::ZERO, vec2(812.0, 392.0));
        let ctx = egui::Context::default();
        let mut view = TerminalView::default();
        let frame = |view: &mut TerminalView, events: Vec<egui::Event>| -> Vec<String> {
            let input = egui::RawInput { screen_rect: Some(rect), events, ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| {
                let id = ui.scope_id().with("pane");
                view.show(&terminal, ui, rect, id, &metrics, &palette);
            });
            output.textures_delta.clear();
            output
                .shapes
                .iter()
                .filter_map(|clipped| match &clipped.shape {
                    egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                    _ => None,
                })
                .collect()
        };
        let pill = |texts: &[String]| texts.iter().any(|t| t.starts_with("Typing from"));
        assert!(!pill(&frame(&mut view, Vec::new())), "nobody types remotely: no pill");
        assert!(view.take_back.is_none());

        let handle = terminal.screen_handle();
        let control: &dyn TerminalControl = &handle;
        let (phone, policy, now) =
            (Client { id: 7, label: "Safari on iPhone".into() }, Policy::default(), std::time::Instant::now());
        control.acquire(&phone, false, now, &policy).unwrap();
        let texts = frame(&mut view, Vec::new());
        assert!(texts.iter().any(|t| t == "Typing from Safari on iPhone"), "{texts:?}");
        let token = control.view("s", &phone, &policy).1.lease.unwrap().0;
        control.resize_for(&phone, token, Some(GridSize::new(40, 12)), now, &policy).unwrap();
        let texts = frame(&mut view, Vec::new());
        assert!(texts.iter().any(|t| t == "Typing from Safari on iPhone · sized for it"), "{texts:?}");
        assert_eq!(terminal.term.lock().grid().columns(), 40, "the pane shows the phone's size meanwhile");

        // One click on Take back ends the lease and the phone's size with it.
        let at = view.take_back.expect("the pill's button").center();
        frame(&mut view, vec![egui::Event::PointerMoved(at)]);
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        frame(&mut view, vec![button(true)]);
        frame(&mut view, vec![button(false)]);
        let (_, state) = control.view("s", &phone, &policy);
        assert_eq!((state.holder, state.reason), (None, Some(LeaseReason::Desktop)));
        assert_eq!(terminal.term.lock().grid().columns(), 100, "back to the pane's size");
        assert!(!pill(&frame(&mut view, Vec::new())), "the pill goes with the lease");
        terminal.shutdown();
    }

    fn selected_terminal() -> (Terminal, TerminalView, Metrics, crate::test_support::TempDir) {
        let dir = crate::test_support::tempdir().unwrap();
        let theme = Theme::from_config(&atomics_core::config::ThemeConfig::default());
        // The status query is a PTY barrier: the shell sees the terminal's four-byte
        // response only after the initial output has been parsed and its Wakeup sent.
        // Seeing "reviewed" alone can race the newline arriving in a later PTY read,
        // before the selected row's final contents have arrived.
        let terminal = Terminal::spawn(crate::host::Wake::default(), theme.terminal_colors(), "/bin/sh".into(), vec!["-c".into(), "stty -echo -icanon min 1 time 0; printf 'reviewed\\r\\n\\033[5n'; dd bs=1 count=4 of=/dev/null 2>/dev/null; stty icanon; : > ready; read -r ignored; printf 'one\\r\\ntwo\\r\\nthree\\r\\nfour\\r\\nfive\\r\\n'; exec sleep 30".into()], dir.path(), HashMap::new(), 100).unwrap();
        let deadline = std::time::Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(3));
        while !dir.path().join("ready").exists() {
            assert!(std::time::Instant::now() < deadline, "terminal did not answer the initial status query");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(terminal.screen_text(24).contains("reviewed"));
        let metrics = Metrics { font: FontId::monospace(12.0), cell: vec2(8.0, 16.0) };
        let mut view = TerminalView::default();
        view.resize(&terminal, Rect::from_min_size(Pos2::ZERO, vec2(160.0, 64.0)), &metrics);
        {
            let term = terminal.term.lock();
            view.sync_selection(&terminal, &term);
            let mut selection = Selection::new(SelectionType::Simple, Point::new(Line(0), Column(0)), Side::Left);
            selection.update(Point::new(Line(0), Column(7)), Side::Right);
            view.selection = Some(selection);
            view.remember_selection(&term);
        }
        (terminal, view, metrics, dir)
    }

    fn wait_for_text(terminal: &Terminal, text: &str) {
        let deadline = std::time::Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(3));
        while !terminal.screen_text(24).contains(text) {
            assert!(std::time::Instant::now() < deadline, "terminal did not output {text}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn copy_selection(terminal: &Terminal, view: &mut TerminalView) -> Option<String> {
        let ctx = egui::Context::default();
        let output = ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::ModifiersChanged(Modifiers::CTRL), egui::Event::Copy],
                ..Default::default()
            },
            |ui| view.handle_keys(terminal, ui),
        );
        let copied = output.platform_output.commands.iter().find_map(|command| match command {
            egui::OutputCommand::CopyText(text) => Some(text.clone()),
            _ => None,
        });
        output.drop_without_applying_deltas();
        copied
    }

    fn raw_input_terminal() -> (Terminal, crate::test_support::TempDir) {
        raw_input_terminal_with_output("")
    }

    fn raw_input_terminal_with_output(output: &str) -> (Terminal, crate::test_support::TempDir) {
        let dir = crate::test_support::tempdir().unwrap();
        // The status response is a barrier for the initial output and its revision. Raw mode
        // lets the file capture control bytes without the line discipline consuming them.
        let script = format!(
            "stty raw -echo; printf 'alpha beta gamma\\r\\n\\033[5n'; \
                      dd bs=1 count=4 of=/dev/null 2>/dev/null; {output}exec cat > input"
        );
        let terminal = Terminal::spawn(
            crate::host::Wake::default(),
            Vec::new(),
            "/bin/sh".into(),
            vec!["-c".into(), script],
            dir.path(),
            HashMap::new(),
            100,
        )
        .unwrap();
        let deadline = std::time::Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(3));
        while !dir.path().join("input").exists() {
            assert!(std::time::Instant::now() < deadline, "terminal did not answer the initial status query");
            std::thread::sleep(Duration::from_millis(5));
        }
        (terminal, dir)
    }

    fn captured_input(terminal: &Terminal, dir: &crate::test_support::TempDir) -> Vec<u8> {
        // A trailing marker proves every preceding write reached the raw reader, including
        // when an event should have written nothing. Each test captures once per terminal.
        terminal.write(vec![0]);
        let deadline = std::time::Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(3));
        loop {
            let mut bytes = std::fs::read(dir.path().join("input")).unwrap();
            if bytes.last() == Some(&0) {
                bytes.pop();
                return bytes;
            }
            assert!(std::time::Instant::now() < deadline, "terminal did not consume the input marker");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn select_input_text(terminal: &Terminal, view: &mut TerminalView) {
        let term = terminal.term.lock();
        view.sync_selection(terminal, &term);
        let mut selection = Selection::new(SelectionType::Simple, Point::new(Line(0), Column(0)), Side::Left);
        selection.update(Point::new(Line(0), Column(15)), Side::Right);
        view.selection = Some(selection);
        view.remember_selection(&term);
    }

    #[test]
    fn copy_without_a_selection_sends_interrupt_off_macos() {
        let (terminal, dir) = raw_input_terminal();
        let mut view = TerminalView::default();
        assert_eq!(copy_selection(&terminal, &mut view), None);
        let bytes = captured_input(&terminal, &dir);
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(bytes, b"\x03");
            assert!(terminal.last_typed().is_some());
        }
        #[cfg(target_os = "macos")]
        {
            assert!(bytes.is_empty());
            assert!(terminal.last_typed().is_none());
        }
    }

    #[test]
    fn copy_keeps_selected_text_while_the_terminal_keeps_printing_and_sends_no_interrupt() {
        let (terminal, dir) = raw_input_terminal_with_output(
            "(while [ ! -e print ]; do sleep 0.01; done; \
             i=0; while [ ! -e stop ]; do i=$((i + 1)); printf '\\rprint %s' \"$i\"; sleep 0.01; done) & ",
        );
        let mut view = TerminalView::default();
        select_input_text(&terminal, &mut view);
        std::fs::write(dir.path().join("print"), "").unwrap();
        wait_for_text(&terminal, "print");
        // Synchronize the selection across several output revisions, as mouse handling and painting do.
        for _ in 0..3 {
            let before = terminal.revision();
            let deadline = std::time::Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(3));
            while terminal.revision() == before {
                assert!(std::time::Instant::now() < deadline, "terminal stopped printing");
                std::thread::sleep(Duration::from_millis(5));
            }
            let term = terminal.term.lock();
            view.sync_selection(&terminal, &term);
            assert!(view.selection.is_some(), "output below the selected row keeps the selection");
        }
        assert_eq!(copy_selection(&terminal, &mut view).as_deref(), Some("alpha beta gamma"));
        assert_eq!(copy_selection(&terminal, &mut view), None, "a held copy key may repeat");
        std::fs::write(dir.path().join("stop"), "").unwrap();
        assert!(captured_input(&terminal, &dir).is_empty(), "copy and its repeats send no bytes");
        assert!(terminal.last_typed().is_none());
    }

    #[test]
    fn a_dedicated_copy_key_without_a_selection_sends_no_interrupt() {
        let (terminal, dir) = raw_input_terminal();
        let mut view = TerminalView::default();
        egui::Context::default()
            .run_ui(egui::RawInput { events: vec![egui::Event::Copy], ..Default::default() }, |ui| {
                view.handle_keys(&terminal, ui)
            })
            .drop_without_applying_deltas();
        assert!(captured_input(&terminal, &dir).is_empty());
        assert!(terminal.last_typed().is_none());
    }

    /// One frame of `events` on the context the view keeps being drawn in.
    fn keys(
        ctx: &egui::Context,
        terminal: &Terminal,
        view: &mut TerminalView,
        events: Vec<egui::Event>,
    ) -> Option<String> {
        let output = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| view.handle_keys(terminal, ui));
        let copied = output.platform_output.commands.iter().find_map(|command| match command {
            egui::OutputCommand::CopyText(text) => Some(text.clone()),
            _ => None,
        });
        output.drop_without_applying_deltas();
        copied
    }

    fn c_up(modifiers: Modifiers) -> egui::Event {
        egui::Event::Key { key: Key::C, physical_key: Some(Key::C), pressed: false, repeat: false, modifiers }
    }

    fn ctrl(on: bool) -> egui::Event {
        egui::Event::ModifiersChanged(if on { Modifiers::CTRL } else { Modifiers::NONE })
    }

    #[test]
    fn a_ctrl_c_quicker_than_a_frame_still_interrupts_off_macos() {
        let (terminal, dir) = raw_input_terminal();
        let mut view = TerminalView::default();
        let ctx = egui::Context::default();
        keys(&ctx, &terminal, &mut view, vec![ctrl(true), egui::Event::Copy, c_up(Modifiers::CTRL), ctrl(false)]);
        let bytes = captured_input(&terminal, &dir);
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(bytes, b"\x03");
            assert!(terminal.last_typed().is_some());
        }
        #[cfg(target_os = "macos")]
        assert!(bytes.is_empty());
    }

    #[test]
    fn a_ctrl_c_whose_ctrl_is_let_go_in_the_frame_of_the_c_still_interrupts_off_macos() {
        let (terminal, dir) = raw_input_terminal();
        let mut view = TerminalView::default();
        let ctx = egui::Context::default();
        keys(&ctx, &terminal, &mut view, vec![ctrl(true)]);
        keys(&ctx, &terminal, &mut view, vec![egui::Event::Copy, ctrl(false)]);
        let bytes = captured_input(&terminal, &dir);
        #[cfg(not(target_os = "macos"))]
        assert_eq!(bytes, b"\x03");
        #[cfg(target_os = "macos")]
        assert!(bytes.is_empty());
    }

    #[test]
    fn two_ctrl_cs_inside_one_frame_interrupt_twice_off_macos() {
        let (terminal, dir) = raw_input_terminal();
        let mut view = TerminalView::default();
        let ctx = egui::Context::default();
        let chord = [ctrl(true), egui::Event::Copy, c_up(Modifiers::CTRL), ctrl(false)];
        keys(&ctx, &terminal, &mut view, chord.iter().chain(&chord).cloned().collect());
        let bytes = captured_input(&terminal, &dir);
        #[cfg(not(target_os = "macos"))]
        assert_eq!(bytes, b"\x03\x03");
        #[cfg(target_os = "macos")]
        assert!(bytes.is_empty());
    }

    #[test]
    fn a_quick_ctrl_c_on_a_selection_copies_and_only_the_next_one_interrupts() {
        let (terminal, dir) = raw_input_terminal();
        let mut view = TerminalView::default();
        let ctx = egui::Context::default();
        select_input_text(&terminal, &mut view);
        let chord = vec![ctrl(true), egui::Event::Copy, c_up(Modifiers::CTRL), ctrl(false)];
        assert_eq!(keys(&ctx, &terminal, &mut view, chord.clone()).as_deref(), Some("alpha beta gamma"));
        assert!(terminal.last_typed().is_none(), "copying text is not terminal input");
        assert_eq!(keys(&ctx, &terminal, &mut view, chord), None);
        let bytes = captured_input(&terminal, &dir);
        #[cfg(not(target_os = "macos"))]
        assert_eq!(bytes, b"\x03");
        #[cfg(target_os = "macos")]
        assert!(bytes.is_empty());
    }

    #[test]
    fn a_copy_repeat_in_the_frame_ctrl_is_let_go_stays_part_of_the_copy() {
        let (terminal, dir) = raw_input_terminal();
        let mut view = TerminalView::default();
        let ctx = egui::Context::default();
        select_input_text(&terminal, &mut view);
        assert_eq!(
            keys(&ctx, &terminal, &mut view, vec![ctrl(true), egui::Event::Copy]).as_deref(),
            Some("alpha beta gamma")
        );
        assert_eq!(keys(&ctx, &terminal, &mut view, vec![egui::Event::Copy, ctrl(false)]), None);
        assert!(captured_input(&terminal, &dir).is_empty(), "the same key press: no interrupt");
        assert!(terminal.last_typed().is_none());
    }

    #[test]
    fn a_dedicated_copy_key_after_ctrl_was_let_go_sends_no_interrupt() {
        let (terminal, dir) = raw_input_terminal();
        let mut view = TerminalView::default();
        let ctx = egui::Context::default();
        keys(&ctx, &terminal, &mut view, vec![ctrl(true), ctrl(false), egui::Event::Copy]);
        keys(&ctx, &terminal, &mut view, vec![egui::Event::Copy]);
        assert!(captured_input(&terminal, &dir).is_empty());
    }

    #[test]
    fn copy_repeats_never_interrupt_and_a_fresh_press_interrupts_off_macos() {
        let (terminal, dir) = raw_input_terminal();
        let mut view = TerminalView::default();
        select_input_text(&terminal, &mut view);
        assert_eq!(copy_selection(&terminal, &mut view).as_deref(), Some("alpha beta gamma"));
        assert!(view.selection.is_none());
        assert!(terminal.last_typed().is_none(), "copying text is not terminal input");
        for _ in 0..3 {
            assert_eq!(copy_selection(&terminal, &mut view), None);
        }
        assert!(terminal.last_typed().is_none(), "repeating a copy is not terminal input");
        let ctx = egui::Context::default();
        ctx.run_ui(
            egui::RawInput {
                events: vec![
                    egui::Event::ModifiersChanged(Modifiers::CTRL),
                    egui::Event::Key {
                        key: Key::C,
                        physical_key: Some(Key::C),
                        pressed: false,
                        repeat: false,
                        modifiers: Modifiers::CTRL,
                    },
                ],
                ..Default::default()
            },
            |ui| view.handle_keys(&terminal, ui),
        )
        .drop_without_applying_deltas();
        // Only a fresh press may send an interrupt.
        terminal.write(b"!".to_vec());
        assert_eq!(copy_selection(&terminal, &mut view), None);
        let bytes = captured_input(&terminal, &dir);
        #[cfg(not(target_os = "macos"))]
        assert_eq!(bytes, b"!\x03");
        #[cfg(target_os = "macos")]
        assert_eq!(bytes, b"!");
    }

    #[test]
    fn cut_sends_control_x_off_macos_with_or_without_a_selection() {
        for selected in [false, true] {
            let (terminal, dir) = raw_input_terminal();
            let mut view = TerminalView::default();
            if selected {
                select_input_text(&terminal, &mut view);
            }
            let ctx = egui::Context::default();
            let output = ctx.run_ui(egui::RawInput { events: vec![egui::Event::Cut], ..Default::default() }, |ui| {
                view.handle_keys(&terminal, ui)
            });
            assert!(
                !output
                    .platform_output
                    .commands
                    .iter()
                    .any(|command| matches!(command, egui::OutputCommand::CopyText(_)))
            );
            output.drop_without_applying_deltas();
            let bytes = captured_input(&terminal, &dir);
            #[cfg(not(target_os = "macos"))]
            {
                assert_eq!(bytes, b"\x18");
                assert!(terminal.last_typed().is_some());
            }
            #[cfg(target_os = "macos")]
            {
                assert!(bytes.is_empty());
                assert!(terminal.last_typed().is_none());
                assert_eq!(view.selection.is_some(), selected);
            }
        }
    }

    #[test]
    fn drag_selection_starts_at_the_press_before_drag_detection() {
        let (terminal, _dir) = raw_input_terminal();
        let mut view = TerminalView::default();
        let metrics = Metrics { font: FontId::monospace(12.0), cell: vec2(8.0, 16.0) };
        let rect = Rect::from_min_size(Pos2::ZERO, vec2(160.0, 64.0));
        let ctx = egui::Context::default();
        let frame = |view: &mut TerminalView, events| {
            let mut started = false;
            ctx.run_ui(egui::RawInput { screen_rect: Some(rect), events, ..Default::default() }, |ui| {
                let response = ui.interact(rect, egui::Id::NULL.with("drag selection"), Sense::click_and_drag());
                started |= response.drag_started();
                view.handle_mouse(&terminal, ui, &response, rect, &metrics);
            })
            .drop_without_applying_deltas();
            started
        };
        let press = egui::pos2(1.0, 8.0);
        frame(&mut view, vec![egui::Event::PointerMoved(press)]);
        frame(
            &mut view,
            vec![egui::Event::PointerButton {
                pos: press,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            }],
        );
        assert!(frame(&mut view, vec![egui::Event::PointerMoved(press + vec2(16.0, 0.0))]));
        frame(&mut view, vec![egui::Event::PointerMoved(egui::pos2(127.0, 8.0))]);
        assert_eq!(copy_selection(&terminal, &mut view).as_deref(), Some("alpha beta gamma"));
    }

    #[test]
    fn selection_is_discarded_after_output_scrolls_without_a_frame() {
        let (terminal, mut view, _, _dir) = selected_terminal();
        assert!(view.selection.is_some());
        terminal.write(b"\r".to_vec());
        // Output scrolls on the PTY thread while the client consumes no frames.
        wait_for_text(&terminal, "five");
        assert!(!terminal.screen_text(4).contains("reviewed"));
        assert_eq!(copy_selection(&terminal, &mut view), None);
        assert!(view.selection.is_none());
    }

    #[test]
    fn selection_is_discarded_when_geometry_changes() {
        let (terminal, mut view, metrics, _dir) = selected_terminal();
        assert!(view.selection.is_some());
        view.resize(&terminal, Rect::from_min_size(Pos2::ZERO, vec2(64.0, 48.0)), &metrics);
        assert_eq!(copy_selection(&terminal, &mut view), None);
        assert!(view.selection.is_none());
    }

    #[test]
    fn color_queries_answer_the_theme_and_follow_theme_changes() {
        let paper =
            Theme::from_config(&atomics_core::config::ThemeConfig { name: "paper".into(), ..Default::default() });
        let claude = Theme::from_config(&atomics_core::config::ThemeConfig::default());
        let (paper_colors, claude_colors) = (paper.palette(), claude.palette());
        let answer = |color: Color32| {
            let channel = |v: u8| format!("{v:02x}{v:02x}");
            format!("rgb:{}/{}/{}!", channel(color.r()), channel(color.g()), channel(color.b()))
        };
        let dir = crate::test_support::tempdir().unwrap();
        // OSC 10 foreground, 11 background, 4;1 red, 12 cursor; each answer has a fixed length.
        for (query, length, light, dark) in [
            ("\\033]10;?\\007", 24, paper_colors.foreground, claude_colors.foreground),
            ("\\033]11;?\\007", 24, paper_colors.terminal, claude_colors.terminal),
            ("\\033]4;1;?\\007", 25, paper_colors.ansi[1], claude_colors.ansi[1]),
            ("\\033]12;?\\007", 24, paper_colors.cursor, claude_colors.cursor),
        ] {
            let terminal = Terminal::spawn(
                crate::host::Wake::default(),
                paper.terminal_colors(),
                "/bin/sh".into(),
                color_query_args(query, length),
                dir.path(),
                HashMap::new(),
                100,
            )
            .unwrap();
            wait_for_text(&terminal, "ready");
            terminal.write(b"x".to_vec());
            wait_for_text(&terminal, &answer(light));
            terminal.set_colors(claude.terminal_colors());
            terminal.write(b"x".to_vec());
            wait_for_text(&terminal, &answer(dark));
            terminal.shutdown();
        }
        assert_eq!(paper.colorfgbg(), "0;15");
        assert_eq!(claude.colorfgbg(), "15;0");
    }

    #[test]
    fn terminals_answer_the_colors_the_painter_draws() {
        let mut custom = atomics_core::config::ThemeConfig { name: "tide".into(), ..Default::default() };
        custom.tokens.cursor = Some("#00ff00".into());
        custom.tokens.ansi = Some(vec!["#102030".into(), "not a color".into()]);
        let builtins = atomics_core::theme::builtin_names()
            .map(|name| atomics_core::config::ThemeConfig { name: name.into(), ..Default::default() });
        for config in builtins.chain([custom]) {
            let theme = Theme::from_config(&config);
            let palette = theme.palette();
            let painted =
                (0..256).map(|i| palette.indexed(i)).chain([palette.foreground, palette.terminal, palette.cursor]);
            let answered = theme.terminal_colors();
            assert_eq!(answered.len(), 259);
            assert_eq!(slot_colors(&palette), answered, "{}: the desktop copy's palette", config.name);
            for (index, (rgb, color)) in answered.iter().zip(painted).enumerate() {
                assert_eq!([rgb.r, rgb.g, rgb.b], [color.r(), color.g(), color.b()], "{} color {index}", config.name);
            }
        }
    }

    #[test]
    fn enter_hands_back_the_typed_line() {
        let none = Modifiers::NONE;
        let mut line = "summarise thx".to_string();
        assert_eq!(typed_key(&mut line, Key::Backspace, none), None);
        line.push_str("is");
        assert_eq!(typed_key(&mut line, Key::Enter, Modifiers::SHIFT), None);
        line.push_str("paper");
        assert_eq!(typed_key(&mut line, Key::Enter, none).as_deref(), Some("summarise this\npaper"));
        assert_eq!(typed_key(&mut line, Key::Enter, none), None, "an empty line sends nothing");
        line.push_str("rm -rf");
        assert_eq!(typed_key(&mut line, Key::C, Modifiers::CTRL), None);
        assert_eq!(typed_key(&mut line, Key::Enter, none), None, "Ctrl+C dropped the line");
    }
}

#[cfg(test)]
mod isolation_tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn children_cannot_inherit_instance_routing() {
        let args = runtime::isolated_args("/usr/bin/env".into(), Vec::new(), None, None);
        let output = crate::test_support::output(
            std::process::Command::new("/usr/bin/env")
                .args(args)
                .env("ATOMICS_SOCKET", "/parent/socket")
                .env("ATOMICS_HOST_SOCKET", "/parent/socket")
                .env("ATOMICS_PANE", "parent-pane")
                .env("ATOMICS_HOOK_PANE", "parent-pane")
                .env("ATOMICS_HOME", "/parent/home")
                .env("ATOMICS_CONFIG", "/parent/config")
                .env("ATOMICS_AZURE_BEARER", "parent-placeholder")
                .env("ATOMICS_AZURE_RELAY_URL", "http://parent-relay")
                .env("TERM_PROGRAM", "atomics"),
        );
        assert!(output.status.success());
        let env = String::from_utf8(output.stdout).unwrap();
        for key in [
            "ATOMICS_SOCKET",
            "ATOMICS_HOST_SOCKET",
            "ATOMICS_PANE",
            "ATOMICS_HOME",
            "ATOMICS_CONFIG",
            "ATOMICS_AZURE_BEARER",
            "ATOMICS_AZURE_RELAY_URL",
            "ATOMICS_HOOK_PANE",
        ] {
            assert!(!env.lines().any(|line| line.starts_with(&format!("{key}="))), "{env}");
        }
        assert!(env.contains("TERM_PROGRAM=atomics"));
    }

    #[test]
    fn a_session_gets_its_own_hook_pane_instead_of_the_parents() {
        let args = runtime::isolated_args("/usr/bin/env".into(), Vec::new(), None, Some("own-pane".into()));
        let output = crate::test_support::output(
            std::process::Command::new("/usr/bin/env").args(args).env("ATOMICS_HOOK_PANE", "parent-pane"),
        );
        assert!(output.status.success());
        let env = String::from_utf8(output.stdout).unwrap();
        let panes: Vec<&str> = env.lines().filter(|line| line.starts_with("ATOMICS_HOOK_PANE=")).collect();
        assert_eq!(panes, ["ATOMICS_HOOK_PANE=own-pane"], "{env}");
    }

    #[test]
    fn a_terminal_reaches_its_own_host_instead_of_the_parents() {
        let args = runtime::isolated_args("/usr/bin/env".into(), Vec::new(), Some("/run/host/atomics.sock"), None);
        let output = crate::test_support::output(
            std::process::Command::new("/usr/bin/env")
                .args(args)
                .env("ATOMICS_SOCKET", "/parent/socket")
                .env("ATOMICS_HOST_SOCKET", "/parent/socket"),
        );
        assert!(output.status.success());
        let env = String::from_utf8(output.stdout).unwrap();
        let routing: Vec<&str> =
            env.lines().filter(|line| line.starts_with("ATOMICS_") && line.contains("_SOCKET=")).collect();
        assert_eq!(
            routing,
            ["ATOMICS_SOCKET=/run/host/atomics.sock", "ATOMICS_HOST_SOCKET=/run/host/atomics.sock"],
            "{env}"
        );
    }

    /// Every terminal a host starts names that host's socket, even where nothing above it does: a login
    /// shell on NixOS that skips the profile's session variables, or a host with a socket of its own.
    #[test]
    fn every_terminal_a_host_starts_names_its_socket() {
        let dir = crate::test_support::tempdir().unwrap();
        let record = dir.path().join("record");
        let script = format!(
            "printf '%s|%s' \"${{ATOMICS_SOCKET-unset}}\" \"${{ATOMICS_HOST_SOCKET-unset}}\" > {}.part && mv {0}.part {0}",
            record.display()
        );
        let env = HashMap::from([("ATOMICS_SOCKET".to_string(), "/parent/socket".to_string())]);
        let _terminal = Terminal::spawn(
            crate::host::Wake::default(),
            Vec::new(),
            "/bin/sh".into(),
            vec!["-c".into(), script],
            dir.path(),
            env,
            100,
        )
        .unwrap();
        let deadline =
            std::time::Instant::now() + crate::test_support::scaled_timeout(std::time::Duration::from_secs(10));
        while !record.exists() {
            assert!(std::time::Instant::now() < deadline, "the terminal never ran");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let socket = atomics_core::paths::socket_path().display().to_string();
        assert_eq!(std::fs::read_to_string(&record).unwrap(), format!("{socket}|{socket}"));
    }
}
