//! An embedded terminal: alacritty's emulator and PTY event loop, drawn with egui.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};
use anyhow::Result;
use egui::{Color32, FontId, Key, Modifiers, Pos2, Rect, Sense, Stroke, Vec2, pos2, vec2};

use crate::theme::{Palette, is_app_chord};

pub struct Shared {
    pub title: Mutex<Option<String>>,
    pub exited: AtomicBool,
    pub bell: AtomicBool,
    size: Mutex<WindowSize>,
    /// The pass that last drew the terminal, and whether it had keyboard focus then.
    drawn: AtomicU64,
    focused: AtomicBool,
    typed: Mutex<Option<Instant>>,
}

/// How long output may wait to be drawn: in the focused terminal, in the others on
/// screen, and off screen, where it only keeps the agent's status current.
const FOCUSED_REPAINT: Duration = Duration::from_millis(100);
const OUTPUT_REPAINT: Duration = Duration::from_millis(200);
const HIDDEN_REPAINT: Duration = Duration::from_millis(500);
/// Output this soon after a key press is its echo and is drawn at once.
const ECHO_WINDOW: Duration = Duration::from_secs(1);

#[derive(Clone)]
struct Listener {
    ctx: egui::Context,
    sender: Arc<OnceLock<EventLoopSender>>,
    shared: Arc<Shared>,
    palette: Arc<Palette>,
}

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let write = |text: String| {
            if let Some(sender) = self.sender.get() {
                let _ = sender.send(Msg::Input(Cow::Owned(text.into_bytes())));
            }
        };
        match event {
            TermEvent::Wakeup => self.repaint(),
            TermEvent::MouseCursorDirty | TermEvent::CursorBlinkingChange => self.ctx.request_repaint(),
            TermEvent::Title(title) => *self.shared.title.lock().unwrap() = Some(title),
            TermEvent::ResetTitle => *self.shared.title.lock().unwrap() = None,
            TermEvent::PtyWrite(text) => write(text),
            TermEvent::ColorRequest(index, format) => write(format(self.palette.indexed_rgb(index))),
            TermEvent::TextAreaSizeRequest(format) => write(format(*self.shared.size.lock().unwrap())),
            TermEvent::ClipboardStore(_, text) => self.ctx.copy_text(text),
            TermEvent::Bell => self.shared.bell.store(true, Ordering::Relaxed),
            TermEvent::ChildExit(_) | TermEvent::Exit => {
                self.shared.exited.store(true, Ordering::Relaxed);
                self.ctx.request_repaint();
            }
            TermEvent::ClipboardLoad(..) => {}
        }
    }
}

impl Listener {
    fn repaint(&self) {
        let shared = &self.shared;
        let on_screen = shared.drawn.load(Ordering::Relaxed) + 2 >= self.ctx.cumulative_pass_nr();
        let focused = on_screen && shared.focused.load(Ordering::Relaxed);
        let echo = focused && shared.typed.lock().unwrap().is_some_and(|at| at.elapsed() < ECHO_WINDOW);
        if echo {
            self.ctx.request_repaint();
        } else {
            let wait = match (on_screen, focused) {
                (true, true) => FOCUSED_REPAINT,
                (true, false) => OUTPUT_REPAINT,
                (false, _) => HIDDEN_REPAINT,
            };
            self.ctx.request_repaint_after(wait);
        }
    }
}

pub struct Terminal {
    process_id: u32,
    term: Arc<FairMutex<Term<Listener>>>,
    sender: EventLoopSender,
    pub shared: Arc<Shared>,
    grid: (usize, usize),
    scroll_remainder: f32,
}

pub struct Metrics {
    pub font: FontId,
    pub cell: Vec2,
}

/// Remove instance routing from every terminal, including inherited bundle settings.
fn isolated_args(program: String, args: Vec<String>) -> Vec<String> {
    ["-u", "IMACS_SOCKET", "-u", "IMACS_PANE", "-u", "IMACS_HOME", "-u", "IMACS_CONFIG"]
        .into_iter()
        .map(str::to_string)
        .chain([program])
        .chain(args)
        .collect()
}

impl Terminal {
    pub fn process_id(&self) -> u32 {
        self.process_id
    }

    pub fn spawn(
        ctx: &egui::Context,
        palette: Arc<Palette>,
        program: String,
        args: Vec<String>,
        cwd: &Path,
        env: HashMap<String, String>,
        scrollback: usize,
    ) -> Result<Terminal> {
        let options = tty::Options {
            shell: Some(tty::Shell::new("/usr/bin/env".into(), isolated_args(program, args))),
            working_directory: Some(cwd.to_path_buf()),
            drain_on_exit: true,
            env,
        };
        let size = WindowSize { num_lines: 24, num_cols: 80, cell_width: 8, cell_height: 16 };
        let pty = tty::new(&options, size, 0)?;
        let process_id = pty.child().id();
        let shared = Arc::new(Shared {
            title: Mutex::new(None),
            exited: AtomicBool::new(false),
            bell: AtomicBool::new(false),
            size: Mutex::new(size),
            drawn: AtomicU64::new(0),
            focused: AtomicBool::new(false),
            typed: Mutex::new(None),
        });
        let listener =
            Listener { ctx: ctx.clone(), sender: Arc::new(OnceLock::new()), shared: shared.clone(), palette };
        let config = TermConfig { scrolling_history: scrollback, ..TermConfig::default() };
        let term = Arc::new(FairMutex::new(Term::new(config, &TermSize::new(80, 24), listener.clone())));
        let event_loop = EventLoop::new(term.clone(), listener.clone(), pty, true, false)?;
        let sender = event_loop.channel();
        let _ = listener.sender.set(sender.clone());
        event_loop.spawn();
        Ok(Terminal { process_id, term, sender, shared, grid: (80, 24), scroll_remainder: 0.0 })
    }

    /// Lines kept above the screen; fewer drops the oldest at once.
    pub fn set_scrollback(&self, lines: usize) {
        self.term.lock().set_options(TermConfig { scrolling_history: lines, ..TermConfig::default() });
    }

    pub fn exited(&self) -> bool {
        self.shared.exited.load(Ordering::Relaxed)
    }

    pub fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        let _ = self.sender.send(Msg::Input(bytes.into()));
    }

    /// Types `text` as one message and presses Enter, the way a person would.
    pub fn submit(&self, text: &str) {
        let bracketed = self.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        self.term.lock().scroll_display(Scroll::Bottom);
        self.write(imacs_core::steer::paste_bytes(text, bracketed));
        // Programs that debounce pastes would read an immediate Enter as part of it.
        let sender = self.sender.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            let _ = sender.send(Msg::Input(Cow::Borrowed(b"\r")));
        });
    }

    /// Types editor commands: each step after a lone Escape, with pauses so the
    /// editor reads Escape as a key of its own rather than as Alt+next key.
    pub fn type_steps(&self, steps: Vec<String>, initial_delay: Duration) {
        let sender = self.sender.clone();
        std::thread::spawn(move || {
            std::thread::sleep(initial_delay);
            for step in steps {
                let _ = sender.send(Msg::Input(Cow::Borrowed(b"\x1b")));
                std::thread::sleep(Duration::from_millis(120));
                let _ = sender.send(Msg::Input(Cow::Owned(step.into_bytes())));
                std::thread::sleep(Duration::from_millis(120));
            }
        });
    }

    /// The visible screen as text, without trailing blank lines; the last `max_lines` of it.
    pub fn screen_text(&self, max_lines: usize) -> String {
        let term = self.term.lock();
        let grid = term.grid();
        let mut lines: Vec<String> = (0..grid.screen_lines())
            .map(|r| {
                let row = &grid[Line(r as i32)];
                let mut text: String = (0..grid.columns())
                    .map(|c| &row[Column(c)])
                    .filter(|cell| !cell.flags.contains(Flags::WIDE_CHAR_SPACER))
                    .map(|cell| if cell.c == '\0' { ' ' } else { cell.c })
                    .collect();
                text.truncate(text.trim_end().len());
                text
            })
            .collect();
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        let skip = lines.len().saturating_sub(max_lines);
        lines[skip..].join("\n")
    }

    pub fn shutdown(&self) {
        let _ = self.sender.send(Msg::Shutdown);
    }

    /// Draws the terminal into `rect` and, when focused, feeds it keyboard and mouse input.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        id: egui::Id,
        metrics: &Metrics,
        palette: &Palette,
    ) -> egui::Response {
        let response = ui.interact(rect, id, Sense::click_and_drag());
        if response.clicked() || response.drag_started() {
            response.request_focus();
        }
        let focused = response.has_focus();
        self.shared.drawn.store(ui.ctx().cumulative_pass_nr(), Ordering::Relaxed);
        self.shared.focused.store(focused, Ordering::Relaxed);
        if focused {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    id,
                    egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
                )
            });
        }

        let inner = rect.shrink2(vec2(6.0, 4.0));
        self.resize(inner, metrics);
        self.handle_mouse(ui, &response, inner, metrics);
        if focused {
            self.handle_keys(ui);
        }
        self.paint(ui, rect, inner, metrics, palette, focused);
        response
    }

    fn resize(&mut self, rect: Rect, metrics: &Metrics) {
        let cols = ((rect.width() / metrics.cell.x).floor() as usize).max(2);
        let rows = ((rect.height() / metrics.cell.y).floor() as usize).max(1);
        if (cols, rows) == self.grid {
            return;
        }
        self.grid = (cols, rows);
        self.term.lock().resize(TermSize::new(cols, rows));
        let size = WindowSize {
            num_lines: rows as u16,
            num_cols: cols as u16,
            cell_width: metrics.cell.x as u16,
            cell_height: metrics.cell.y as u16,
        };
        *self.shared.size.lock().unwrap() = size;
        let _ = self.sender.send(Msg::Resize(size));
    }

    fn point_at(&self, pos: Pos2, rect: Rect, metrics: &Metrics, offset: usize) -> (Point, Side) {
        let x = ((pos.x - rect.left()) / metrics.cell.x).max(0.0);
        let col = (x.floor() as usize).min(self.grid.0 - 1);
        let row = (((pos.y - rect.top()) / metrics.cell.y).max(0.0).floor() as usize).min(self.grid.1 - 1);
        let side = if x.fract() < 0.5 { Side::Left } else { Side::Right };
        (Point::new(Line(row as i32 - offset as i32), Column(col)), side)
    }

    fn handle_mouse(&mut self, ui: &egui::Ui, response: &egui::Response, rect: Rect, metrics: &Metrics) {
        let mut term = self.term.lock();
        let offset = term.grid().display_offset();

        if let Some(pos) = response.interact_pointer_pos() {
            let (point, side) = self.point_at(pos, rect, metrics, offset);
            if response.drag_started() {
                term.selection = Some(Selection::new(SelectionType::Simple, point, side));
            } else if response.dragged()
                && let Some(selection) = term.selection.as_mut()
            {
                selection.update(point, side);
            }
        }
        if response.clicked() {
            term.selection = None;
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
            self.write(seq.into_bytes());
        } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
            let key = if lines > 0 { "\x1bOA" } else { "\x1bOB" };
            drop(term);
            self.write(key.repeat(lines.unsigned_abs() as usize).into_bytes());
        } else {
            term.scroll_display(Scroll::Delta(lines));
        }
    }

    fn handle_keys(&mut self, ui: &egui::Ui) {
        let events = ui.input(|i| i.events.clone());
        let app_cursor = self.term.lock().mode().contains(TermMode::APP_CURSOR);
        let bracketed = self.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        let mut out: Vec<u8> = Vec::new();
        for event in events {
            match event {
                egui::Event::Text(text) => {
                    let mods = ui.input(|i| i.modifiers);
                    if !is_app_chord(&mods) && !mods.ctrl {
                        out.extend_from_slice(text.as_bytes());
                    }
                }
                egui::Event::Paste(text) => out.extend(imacs_core::steer::paste_bytes(&text, bracketed)),
                egui::Event::Copy => {
                    if let Some(text) = self.term.lock().selection_to_string() {
                        ui.ctx().copy_text(text);
                    }
                }
                egui::Event::Key { key, pressed: true, modifiers, .. } if !is_app_chord(&modifiers) => {
                    if let Some(bytes) = key_bytes(key, modifiers, app_cursor) {
                        out.extend(bytes);
                    }
                }
                _ => {}
            }
        }
        if !out.is_empty() {
            *self.shared.typed.lock().unwrap() = Some(Instant::now());
            let mut term = self.term.lock();
            term.scroll_display(Scroll::Bottom);
            term.selection = None;
            drop(term);
            self.write(out);
        }
    }

    fn paint(&self, ui: &egui::Ui, outer: Rect, rect: Rect, metrics: &Metrics, palette: &Palette, focused: bool) {
        let painter = ui.painter_at(outer);
        painter.rect_filled(outer, 0.0, palette.background);

        let term = self.term.lock();
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
            if content.selection.is_some_and(|s| s.contains(point)) {
                bg = palette.selection;
            }
            let width = if c.flags.contains(Flags::WIDE_CHAR) { 2.0 } else { 1.0 };
            if bg != palette.background {
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

impl Drop for Terminal {
    fn drop(&mut self) {
        self.shutdown();
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
            NamedColor::Background => self.background,
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

    fn indexed_rgb(&self, index: usize) -> Rgb {
        let color = match index {
            256 => self.foreground,
            257 => self.background,
            258 => self.cursor,
            i => self.indexed(i),
        };
        Rgb { r: color.r(), g: color.g(), b: color.b() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

#[cfg(test)]
mod isolation_tests {
    use super::*;

    #[test]
    fn children_cannot_inherit_instance_routing() {
        let args = isolated_args("/usr/bin/env".into(), Vec::new());
        let output = std::process::Command::new("/usr/bin/env")
            .args(args)
            .env("IMACS_SOCKET", "/parent/socket")
            .env("IMACS_PANE", "parent-pane")
            .env("IMACS_HOME", "/parent/home")
            .env("IMACS_CONFIG", "/parent/config")
            .env("TERM_PROGRAM", "imacs")
            .output()
            .unwrap();
        assert!(output.status.success());
        let env = String::from_utf8(output.stdout).unwrap();
        for key in ["IMACS_SOCKET", "IMACS_PANE", "IMACS_HOME", "IMACS_CONFIG"] {
            assert!(!env.lines().any(|line| line.starts_with(&format!("{key}="))), "{env}");
        }
        assert!(env.contains("TERM_PROGRAM=imacs"));
    }
}
