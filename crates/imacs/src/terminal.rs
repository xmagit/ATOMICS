//! Native terminal client: painting, keyboard/pointer input and selection.

mod runtime;
pub use runtime::Terminal;

use crate::theme::{Palette, is_app_chord};
use alacritty_terminal::event::WindowSize;
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};
use egui::{Color32, FontId, Key, Modifiers, Pos2, Rect, Sense, Stroke, Vec2, pos2, vec2};

pub struct Metrics {
    pub font: FontId,
    pub cell: Vec2,
}

pub struct TerminalView {
    terminal: Option<std::sync::Weak<runtime::Shared>>,
    grid: (usize, usize),
    scroll_remainder: f32,
    selection: Option<Selection>,
    selection_revision: u64,
    /// What has been typed since the last Enter.
    line: String,
}

impl Default for TerminalView {
    fn default() -> Self {
        Self {
            terminal: None,
            grid: (80, 24),
            scroll_remainder: 0.0,
            selection: None,
            selection_revision: 0,
            line: String::new(),
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
            self.scroll_remainder = 0.0;
            self.line.clear();
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
        }
        self.paint(terminal, ui, (rect, inner), metrics, palette, focused);
        response
    }

    fn resize(&mut self, terminal: &Terminal, rect: Rect, metrics: &Metrics) {
        let cols = ((rect.width() / metrics.cell.x).floor() as usize).max(2);
        let rows = ((rect.height() / metrics.cell.y).floor() as usize).max(1);
        if (cols, rows) == self.grid {
            return;
        }
        self.grid = (cols, rows);
        self.selection = None;
        let size = WindowSize {
            num_lines: rows as u16,
            num_cols: cols as u16,
            cell_width: metrics.cell.x as u16,
            cell_height: metrics.cell.y as u16,
        };
        terminal.resize(size);
    }

    // Client anchors are grid coordinates. Output can scroll or rewrite the
    // cells beneath them, so discard them before any locked query or paint.
    fn sync_selection(&mut self, terminal: &Terminal) {
        let revision = terminal.revision();
        if self.selection_revision != revision {
            self.selection = None;
            self.selection_revision = revision;
        }
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
        self.sync_selection(terminal);
        let offset = term.grid().display_offset();

        if let Some(pos) = response.interact_pointer_pos() {
            let (point, side) = self.point_at(pos, rect, metrics, offset);
            if response.drag_started() {
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
            let button = if lines > 0 { 64 } else { 65 };
            let row = point.line.0 + offset as i32 + 1;
            let seq = format!("\x1b[<{button};{};{row}M", point.column.0 + 1).repeat(lines.unsigned_abs() as usize);
            drop(term);
            terminal.write(seq.into_bytes());
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
        let app_cursor = terminal.term.lock().mode().contains(TermMode::APP_CURSOR);
        let bracketed = terminal.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        let mut out: Vec<u8> = Vec::new();
        for event in events {
            match event {
                egui::Event::Text(text) => {
                    let mods = ui.input(|i| i.modifiers);
                    if !is_app_chord(&mods) && !mods.ctrl {
                        out.extend_from_slice(text.as_bytes());
                        self.line.push_str(&text);
                    }
                }
                egui::Event::Paste(text) => {
                    out.extend(imacs_core::steer::paste_bytes(&text, bracketed));
                    self.line.push_str(&text);
                }
                egui::Event::Copy => {
                    let mut term = terminal.term.lock();
                    self.sync_selection(terminal);
                    // alacritty's copy helper reads its selection field. Install the
                    // client's selection only for this locked query, then restore it.
                    let previous = std::mem::replace(&mut term.selection, self.selection.clone());
                    let text = term.selection_to_string();
                    term.selection = previous;
                    if let Some(text) = text {
                        ui.ctx().copy_text(text);
                    }
                }
                egui::Event::Key { key, pressed: true, modifiers, .. } if !is_app_chord(&modifiers) => {
                    if let Some(bytes) = key_bytes(key, modifiers, app_cursor) {
                        out.extend(bytes);
                        if let Some(line) = typed_key(&mut self.line, key, modifiers) {
                            *terminal.shared.submitted.lock().unwrap() = Some(line);
                        }
                    }
                }
                _ => {}
            }
        }
        if !out.is_empty() {
            terminal.typed();
            let mut term = terminal.term.lock();
            term.scroll_display(Scroll::Bottom);
            self.selection = None;
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
        let painter = ui.painter_at(outer);
        painter.rect_filled(outer, 0.0, palette.terminal);

        let term = terminal.term.lock();
        self.sync_selection(terminal);
        let selection = self.selection.as_ref().and_then(|s| s.to_range(&*term));
        let content = term.renderable_content();
        let colors = content.colors;
        let offset = content.display_offset as i32;
        let cell = metrics.cell;
        let origin = rect.min;
        let at = |row: i32, col: usize| pos2(origin.x + col as f32 * cell.x, origin.y + row as f32 * cell.y);

        let mut run = Run::default();
        for indexed in content.display_iter {
            let point = indexed.point;
            let c = indexed.cell;
            if c.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let row = point.line.0 + offset;
            let col = point.column.0;
            let mut fg = palette.resolve(c.fg, colors);
            let mut bg = palette.resolve(c.bg, colors);
            if c.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if c.flags.contains(Flags::DIM) {
                fg = palette.dim(fg);
            }
            if c.flags.contains(Flags::HIDDEN) {
                fg = bg;
            }
            if selection.is_some_and(|s| s.contains(point)) {
                bg = palette.selection;
            }
            let width = if c.flags.contains(Flags::WIDE_CHAR) { 2.0 } else { 1.0 };
            if bg != palette.terminal {
                painter.rect_filled(Rect::from_min_size(at(row, col), vec2(cell.x * width, cell.y)), 0.0, bg);
            }
            if c.flags.intersects(Flags::ALL_UNDERLINES) {
                let y = at(row, col).y + cell.y - 1.5;
                let x = at(row, col).x;
                painter.line_segment([pos2(x, y), pos2(x + cell.x * width, y)], Stroke::new(1.0, fg));
            }

            let ch = c.c;
            if ch == ' ' || ch == '\0' {
                // Blanks extend a run whatever their color, so a line stays one shape.
                if !run.text.is_empty() && run.row == row && run.next_col == col {
                    run.text.push(' ');
                    run.next_col += 1;
                }
                continue;
            }
            let simple = ch.is_ascii() && width == 1.0;
            if !run.continues(row, col, fg) || !simple {
                run.flush(&painter, &at, metrics);
            }
            if simple {
                if run.text.is_empty() {
                    run = Run { row, col, fg, text: String::new(), next_col: col };
                }
                run.text.push(ch);
                run.next_col = col + 1;
            } else {
                let mut text = String::from(ch);
                if let Some(extra) = c.zerowidth() {
                    text.extend(extra);
                }
                let center = at(row, col) + vec2(cell.x * width / 2.0, cell.y / 2.0);
                painter.text(center, egui::Align2::CENTER_CENTER, text, metrics.font.clone(), fg);
            }
        }
        run.flush(&painter, &at, metrics);

        let cursor = content.cursor;
        let cursor_row = cursor.point.line.0 + offset;
        if cursor.shape != CursorShape::Hidden && cursor_row >= 0 && (cursor_row as usize) < self.grid.1 {
            let r = Rect::from_min_size(at(cursor_row, cursor.point.column.0), cell);
            match (focused, cursor.shape) {
                (true, CursorShape::Beam) => {
                    painter.rect_filled(r.with_max_x(r.left() + 2.0), 0.0, palette.cursor);
                }
                (true, CursorShape::Underline) => {
                    painter.rect_filled(r.with_min_y(r.bottom() - 2.0), 0.0, palette.cursor);
                }
                (true, _) => {
                    painter.rect_filled(r, 0.0, palette.cursor.gamma_multiply(0.7));
                }
                (false, _) => {
                    painter.rect_stroke(
                        r,
                        0.0,
                        Stroke::new(1.0, palette.cursor.gamma_multiply(0.6)),
                        egui::StrokeKind::Inside,
                    );
                }
            }
        }
    }
}

/// Consecutive ASCII cells with the same color, laid out as one text shape.
#[derive(Default)]
struct Run {
    row: i32,
    col: usize,
    next_col: usize,
    fg: Color32,
    text: String,
}

impl Run {
    fn continues(&self, row: i32, col: usize, fg: Color32) -> bool {
        self.text.is_empty() || (self.row == row && self.next_col == col && self.fg == fg)
    }

    fn flush(&mut self, painter: &egui::Painter, at: &impl Fn(i32, usize) -> Pos2, metrics: &Metrics) {
        if self.text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.text);
        let text = text.trim_end().to_string();
        painter.text(at(self.row, self.col), egui::Align2::LEFT_TOP, text, metrics.font.clone(), self.fg);
    }
}

/// Terminal input for keys that don't arrive as text.
fn key_bytes(key: Key, m: Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    let code = 1 + m.shift as u8 + 2 * m.alt as u8 + 4 * m.ctrl as u8;
    let cursor = |c: char| {
        if code > 1 {
            format!("\x1b[1;{code}{c}")
        } else if app_cursor {
            format!("\x1bO{c}")
        } else {
            format!("\x1b[{c}")
        }
    };
    let tilde = |n: u8| if code > 1 { format!("\x1b[{n};{code}~") } else { format!("\x1b[{n}~") };
    let text = match key {
        // Shift/Option+Enter insert a newline in Claude Code and most line editors.
        Key::Enter if m.shift || m.alt => "\x1b\r".to_string(),
        Key::Enter => "\r".to_string(),
        Key::Tab if m.shift => "\x1b[Z".to_string(),
        Key::Tab => "\t".to_string(),
        Key::Backspace if m.alt => "\x1b\x7f".to_string(),
        Key::Backspace if m.ctrl => "\x08".to_string(),
        Key::Backspace => "\x7f".to_string(),
        Key::Escape => "\x1b".to_string(),
        Key::ArrowUp => cursor('A'),
        Key::ArrowDown => cursor('B'),
        Key::ArrowRight => cursor('C'),
        Key::ArrowLeft => cursor('D'),
        Key::Home => cursor('H'),
        Key::End => cursor('F'),
        Key::Insert => tilde(2),
        Key::Delete => tilde(3),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        Key::F1 => "\x1bOP".to_string(),
        Key::F2 => "\x1bOQ".to_string(),
        Key::F3 => "\x1bOR".to_string(),
        Key::F4 => "\x1bOS".to_string(),
        Key::F5 => tilde(15),
        Key::F6 => tilde(17),
        Key::F7 => tilde(18),
        Key::F8 => tilde(19),
        Key::F9 => tilde(20),
        Key::F10 => tilde(21),
        Key::F11 => tilde(23),
        Key::F12 => tilde(24),
        _ if m.ctrl => return ctrl_byte(key).map(|b| vec![b]),
        _ => return None,
    };
    Some(text.into_bytes())
}

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

fn ctrl_byte(key: Key) -> Option<u8> {
    let name = key.name();
    if name.len() == 1 {
        let c = name.chars().next()?.to_ascii_lowercase();
        if c.is_ascii_lowercase() {
            return Some(c as u8 & 0x1f);
        }
    }
    match key {
        Key::Space => Some(0),
        Key::OpenBracket => Some(0x1b),
        Key::Backslash => Some(0x1c),
        Key::CloseBracket => Some(0x1d),
        _ => None,
    }
}

impl Palette {
    fn resolve(&self, color: Color, overrides: &alacritty_terminal::term::color::Colors) -> Color32 {
        let rgb = |rgb: Rgb| Color32::from_rgb(rgb.r, rgb.g, rgb.b);
        match color {
            Color::Spec(spec) => rgb(spec),
            Color::Indexed(index) => overrides[index as usize].map(rgb).unwrap_or_else(|| self.indexed(index as usize)),
            Color::Named(name) => overrides[name].map(rgb).unwrap_or_else(|| self.named(name)),
        }
    }

    fn named(&self, name: NamedColor) -> Color32 {
        match name {
            NamedColor::Foreground | NamedColor::BrightForeground => self.foreground,
            NamedColor::Background => self.terminal,
            NamedColor::Cursor => self.cursor,
            NamedColor::DimForeground => self.dim(self.foreground),
            other => {
                let index = other as usize;
                if index < 16 {
                    self.ansi[index]
                } else {
                    // Dim variants follow the 16 colors in order, starting at DimBlack.
                    let base = index.saturating_sub(NamedColor::DimBlack as usize).min(7);
                    self.dim(self.ansi[base])
                }
            }
        }
    }

    pub fn terminal_colors(&self) -> Vec<Rgb> {
        (0..=258).map(|index| self.indexed_rgb(index)).collect()
    }

    fn indexed_rgb(&self, index: usize) -> Rgb {
        let color = match index {
            256 => self.foreground,
            257 => self.terminal,
            258 => self.cursor,
            i => self.indexed(i),
        };
        Rgb { r: color.r(), g: color.g(), b: color.b() }
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
    use std::{collections::HashMap, sync::Arc, time::Duration};

    #[test]
    fn screen_reader_defers_terminal_lock_to_worker() {
        let palette = Arc::new(Palette::from_config(&imacs_core::config::ThemeConfig::default()));
        let dir = crate::test_support::tempdir().unwrap();
        let terminal = Terminal::spawn(
            crate::host::Wake::default(),
            palette.terminal_colors(),
            "/bin/cat".into(),
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

    fn selected_terminal() -> (Terminal, TerminalView, Metrics, crate::test_support::TempDir) {
        let dir = crate::test_support::tempdir().unwrap();
        let palette = Palette::from_config(&imacs_core::config::ThemeConfig::default());
        let terminal = Terminal::spawn(crate::host::Wake::default(), palette.terminal_colors(), "/bin/sh".into(), vec!["-c".into(), "stty -echo; printf 'reviewed\\r\\n'; read -r ignored; printf 'one\\r\\ntwo\\r\\nthree\\r\\nfour\\r\\nfive\\r\\n'; exec /bin/sleep 30".into()], dir.path(), HashMap::new(), 100).unwrap();
        wait_for_text(&terminal, "reviewed");
        let metrics = Metrics { font: FontId::monospace(12.0), cell: vec2(8.0, 16.0) };
        let mut view = TerminalView::default();
        view.resize(&terminal, Rect::from_min_size(Pos2::ZERO, vec2(160.0, 64.0)), &metrics);
        {
            let _guard = terminal.term.lock();
            view.sync_selection(&terminal);
            let mut selection = Selection::new(SelectionType::Simple, Point::new(Line(0), Column(0)), Side::Left);
            selection.update(Point::new(Line(0), Column(7)), Side::Right);
            view.selection = Some(selection);
        }
        (terminal, view, metrics, dir)
    }

    fn wait_for_text(terminal: &Terminal, text: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !terminal.screen_text(24).contains(text) {
            assert!(std::time::Instant::now() < deadline, "terminal did not output {text}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn copy_selection(terminal: &Terminal, view: &mut TerminalView) -> Option<String> {
        let ctx = egui::Context::default();
        let output = ctx.run_ui(egui::RawInput { events: vec![egui::Event::Copy], ..Default::default() }, |ui| {
            view.handle_keys(terminal, ui)
        });
        let copied = output.platform_output.commands.iter().find_map(|command| match command {
            egui::OutputCommand::CopyText(text) => Some(text.clone()),
            _ => None,
        });
        output.drop_without_applying_deltas();
        copied
    }

    #[test]
    fn selection_is_discarded_after_output_scrolls_without_a_frame() {
        let (terminal, mut view, _, _dir) = selected_terminal();
        assert_eq!(copy_selection(&terminal, &mut view).as_deref(), Some("reviewed"));
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
        assert_eq!(copy_selection(&terminal, &mut view).as_deref(), Some("reviewed"));
        view.resize(&terminal, Rect::from_min_size(Pos2::ZERO, vec2(64.0, 48.0)), &metrics);
        assert_eq!(copy_selection(&terminal, &mut view), None);
        assert!(view.selection.is_none());
    }

    #[test]
    fn color_queries_answer_the_theme_and_follow_theme_changes() {
        let paper =
            Palette::from_config(&imacs_core::config::ThemeConfig { name: "paper".into(), ..Default::default() });
        let claude = Palette::from_config(&imacs_core::config::ThemeConfig::default());
        let answer = |color: Color32| {
            let channel = |v: u8| format!("{v:02x}{v:02x}");
            format!("rgb:{}/{}/{}!", channel(color.r()), channel(color.g()), channel(color.b()))
        };
        let dir = crate::test_support::tempdir().unwrap();
        // OSC 10 foreground, 11 background, 4;1 red, 12 cursor; each answer has a fixed length.
        for (query, length, light, dark) in [
            ("\\033]10;?\\007", 24, paper.foreground, claude.foreground),
            ("\\033]11;?\\007", 24, paper.terminal, claude.terminal),
            ("\\033]4;1;?\\007", 25, paper.ansi[1], claude.ansi[1]),
            ("\\033]12;?\\007", 24, paper.cursor, claude.cursor),
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
    fn keys_map_to_terminal_sequences() {
        let none = Modifiers::NONE;
        assert_eq!(key_bytes(Key::Enter, none, false).unwrap(), b"\r");
        assert_eq!(key_bytes(Key::Enter, Modifiers::SHIFT, false).unwrap(), b"\x1b\r");
        assert_eq!(key_bytes(Key::Tab, Modifiers::SHIFT, false).unwrap(), b"\x1b[Z");
        assert_eq!(key_bytes(Key::ArrowUp, none, false).unwrap(), b"\x1b[A");
        assert_eq!(key_bytes(Key::ArrowUp, none, true).unwrap(), b"\x1bOA");
        assert_eq!(key_bytes(Key::ArrowLeft, Modifiers::ALT, false).unwrap(), b"\x1b[1;3D");
        assert_eq!(key_bytes(Key::C, Modifiers::CTRL, false).unwrap(), [3]);
        assert_eq!(key_bytes(Key::C, none, false), None);
        assert_eq!(key_bytes(Key::Delete, none, false).unwrap(), b"\x1b[3~");
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

    #[test]
    fn children_cannot_inherit_instance_routing() {
        let args = runtime::isolated_args("/usr/bin/env".into(), Vec::new());
        let output = crate::test_support::output(
            std::process::Command::new("/usr/bin/env")
                .args(args)
                .env("IMACS_SOCKET", "/parent/socket")
                .env("IMACS_PANE", "parent-pane")
                .env("IMACS_HOME", "/parent/home")
                .env("IMACS_CONFIG", "/parent/config")
                .env("TERM_PROGRAM", "imacs"),
        );
        assert!(output.status.success());
        let env = String::from_utf8(output.stdout).unwrap();
        for key in ["IMACS_SOCKET", "IMACS_PANE", "IMACS_HOME", "IMACS_CONFIG"] {
            assert!(!env.lines().any(|line| line.starts_with(&format!("{key}="))), "{env}");
        }
        assert!(env.contains("TERM_PROGRAM=imacs"));
    }
}
