//! Host-owned PTY, child and emulator; notifications never depend on egui.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::Rgb;
use anyhow::Result;
pub struct Shared {
    pub title: Mutex<Option<String>>,
    pub exited: AtomicBool,
    pub bell: AtomicBool,
    pub changed: AtomicBool,
    pub clipboard: Mutex<Option<String>>,
    wake: crate::host::Wake,
    size: Mutex<WindowSize>,
    shown: Mutex<Option<(Instant, bool)>>,
    typed: Mutex<Option<Instant>>,
}

#[derive(Clone)]
pub(super) struct Listener {
    sender: Arc<OnceLock<EventLoopSender>>,
    shared: Arc<Shared>,
    colors: Vec<Rgb>,
}

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let write = |text: String| {
            if let Some(sender) = self.sender.get() {
                let _ = sender.send(Msg::Input(Cow::Owned(text.into_bytes())));
            }
        };
        match event {
            TermEvent::Title(title) => *self.shared.title.lock().unwrap() = Some(title),
            TermEvent::ResetTitle => *self.shared.title.lock().unwrap() = None,
            TermEvent::PtyWrite(text) => write(text),
            TermEvent::ColorRequest(index, format) => {
                write(format(self.colors.get(index).copied().unwrap_or(self.colors[256])))
            }
            TermEvent::TextAreaSizeRequest(format) => write(format(*self.shared.size.lock().unwrap())),
            TermEvent::ClipboardStore(_, text) => *self.shared.clipboard.lock().unwrap() = Some(text),
            TermEvent::Bell => self.shared.bell.store(true, Ordering::Relaxed),
            TermEvent::ChildExit(_) | TermEvent::Exit => self.shared.exited.store(true, Ordering::Release),
            TermEvent::ClipboardLoad(..) => return,
            TermEvent::Wakeup | TermEvent::MouseCursorDirty | TermEvent::CursorBlinkingChange => {}
        }
        self.shared.changed.store(true, Ordering::Release);
        self.shared.wake.notify();
    }
}

/// Native runtime handle. Dropping it shuts down the PTY, even if no view was drawn.
pub struct Terminal {
    process_id: u32,
    pub(super) term: Arc<FairMutex<Term<Listener>>>,
    sender: EventLoopSender,
    pub shared: Arc<Shared>,
}

/// Remove instance routing from every terminal, including inherited bundle settings.
pub(super) fn isolated_args(program: String, args: Vec<String>) -> Vec<String> {
    ["-u", "IMACS_SOCKET", "-u", "IMACS_PANE", "-u", "IMACS_HOME", "-u", "IMACS_CONFIG"]
        .into_iter()
        .map(str::to_string)
        .chain([program])
        .chain(args)
        .collect()
}

impl Terminal {
    pub fn shown(&self, focused: bool) {
        *self.shared.shown.lock().unwrap() = Some((Instant::now(), focused));
    }

    pub fn typed(&self) {
        *self.shared.typed.lock().unwrap() = Some(Instant::now());
    }

    /// Output cadence is a host decision informed by native-client visibility.
    /// These hints expire by wall clock, independently of egui frame counters.
    pub fn repaint_delay(&self) -> Duration {
        let shown = *self.shared.shown.lock().unwrap();
        let visible = shown.is_some_and(|(at, _)| at.elapsed() < Duration::from_millis(500));
        let focused = visible && shown.is_some_and(|(_, focused)| focused);
        if focused && self.shared.typed.lock().unwrap().is_some_and(|at| at.elapsed() < Duration::from_secs(1)) {
            Duration::ZERO
        } else if focused {
            Duration::from_millis(100)
        } else if visible {
            Duration::from_millis(200)
        } else {
            Duration::from_millis(500)
        }
    }

    pub fn process_id(&self) -> u32 {
        self.process_id
    }

    pub fn spawn(
        wake: crate::host::Wake,
        colors: Vec<Rgb>,
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
            changed: AtomicBool::new(false),
            clipboard: Mutex::new(None),
            wake,
            size: Mutex::new(size),
            shown: Mutex::new(None),
            typed: Mutex::new(None),
        });
        let listener = Listener { sender: Arc::new(OnceLock::new()), shared: shared.clone(), colors };
        let config = TermConfig { scrolling_history: scrollback, ..TermConfig::default() };
        let term = Arc::new(FairMutex::new(Term::new(config, &TermSize::new(80, 24), listener.clone())));
        let event_loop = EventLoop::new(term.clone(), listener.clone(), pty, true, false)?;
        let sender = event_loop.channel();
        let _ = listener.sender.set(sender.clone());
        event_loop.spawn();
        Ok(Terminal { process_id, term, sender, shared })
    }

    /// Lines kept above the screen; fewer drops the oldest at once.
    pub fn set_scrollback(&self, lines: usize) {
        self.term.lock().set_options(TermConfig { scrolling_history: lines, ..TermConfig::default() });
    }

    pub fn exited(&self) -> bool {
        self.shared.exited.load(Ordering::Acquire)
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
        self.screen_reader(max_lines)()
    }

    pub fn screen_reader(&self, max_lines: usize) -> impl FnOnce() -> String + Send + 'static {
        let term = self.term.clone();
        move || {
            let term = term.lock();
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
    }

    pub fn shutdown(&self) {
        let _ = self.sender.send(Msg::Shutdown);
    }

    pub fn resize(&self, size: WindowSize) {
        self.term.lock().resize(TermSize::new(size.num_cols as usize, size.num_lines as usize));
        *self.shared.size.lock().unwrap() = size;
        let _ = self.sender.send(Msg::Resize(size));
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listener_exit_wakes_the_host_without_a_client() {
        let wake = crate::host::Wake::default();
        let shared = Arc::new(Shared {
            title: Mutex::new(None),
            exited: AtomicBool::new(false),
            bell: AtomicBool::new(false),
            changed: AtomicBool::new(false),
            clipboard: Mutex::new(None),
            wake: wake.clone(),
            size: Mutex::new(WindowSize { num_lines: 24, num_cols: 80, cell_width: 8, cell_height: 16 }),
            shown: Mutex::new(None),
            typed: Mutex::new(None),
        });
        let listener = Listener { sender: Arc::new(OnceLock::new()), shared: shared.clone(), colors: Vec::new() };
        listener.send_event(TermEvent::Exit);
        let now = Instant::now();
        wake.wait_until(now + Duration::from_secs(30));
        assert!(now.elapsed() < Duration::from_secs(1));
        assert!(shared.exited.load(Ordering::Acquire));
        assert!(shared.changed.load(Ordering::Acquire));
    }
}
