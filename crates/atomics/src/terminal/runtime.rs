//! Host-owned PTY, child and emulator; notifications never depend on egui.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use anyhow::Result;

#[derive(Clone, Copy, Default)]
struct InputHistory {
    last_typed: Option<Instant>,
    draft_at: Option<Instant>,
}

impl InputHistory {
    fn record(&mut self, at: Instant, submitted: bool) {
        self.last_typed = Some(at);
        self.draft_at = (!submitted).then_some(at);
    }

    fn empty_since(self, turn_ended: Option<Instant>) -> Option<bool> {
        self.last_typed.map(|_| self.draft_at.is_none_or(|draft| turn_ended.is_some_and(|ended| draft <= ended)))
    }
}

pub struct Shared {
    pub title: Mutex<Option<String>>,
    /// The last line typed into this terminal and sent with Enter.
    pub submitted: Mutex<Option<String>>,
    pub exited: AtomicBool,
    pub bell: AtomicBool,
    pub changed: AtomicBool,
    revision: AtomicU64,
    pub clipboard: Mutex<Option<String>>,
    wake: crate::host::Wake,
    size: Mutex<WindowSize>,
    shown: Mutex<Option<(Instant, bool)>>,
    input: Mutex<InputHistory>,
    /// The theme's 256 colors, foreground, background and cursor, answered to OSC 4/10/11/12.
    colors: Mutex<Vec<Rgb>>,
}

#[derive(Clone)]
pub(super) struct Listener {
    sender: Arc<OnceLock<EventLoopSender>>,
    shared: Arc<Shared>,
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
                let colors = self.shared.colors.lock().unwrap();
                if let Some(color) = colors.get(index).or(colors.get(256)) {
                    write(format(*color));
                }
            }
            TermEvent::TextAreaSizeRequest(format) => write(format(*self.shared.size.lock().unwrap())),
            TermEvent::ClipboardStore(_, text) => *self.shared.clipboard.lock().unwrap() = Some(text),
            TermEvent::Bell => self.shared.bell.store(true, Ordering::Relaxed),
            TermEvent::ChildExit(_) | TermEvent::Exit => self.shared.exited.store(true, Ordering::Release),
            TermEvent::ClipboardLoad(..) => return,
            TermEvent::Wakeup => {
                self.shared.revision.fetch_add(1, Ordering::Release);
            }
            TermEvent::MouseCursorDirty | TermEvent::CursorBlinkingChange => {}
        }
        self.shared.changed.store(true, Ordering::Release);
        self.shared.wake.notify();
    }
}

/// Native runtime handle. Dropping it shuts down the PTY, even if no view was drawn.
pub struct Terminal {
    process_id: u32,
    stopped: AtomicBool,
    pub(super) term: Arc<FairMutex<Term<Listener>>>,
    sender: EventLoopSender,
    pub shared: Arc<Shared>,
}

/// Remove instance routing from every terminal, including inherited bundle settings.
pub(super) fn isolated_args(program: String, args: Vec<String>) -> Vec<String> {
    [
        "-u",
        "ATOMICS_SOCKET",
        "-u",
        "ATOMICS_PANE",
        "-u",
        "ATOMICS_HOME",
        "-u",
        "ATOMICS_CONFIG",
        "-u",
        "ATOMICS_AZURE_BEARER",
        "-u",
        "ATOMICS_AZURE_RELAY_URL",
    ]
    .into_iter()
    .map(str::to_string)
    .chain([program])
    .chain(args)
    .collect()
}

impl Terminal {
    pub(super) fn revision(&self) -> u64 {
        self.shared.revision.load(Ordering::Acquire)
    }

    pub fn shown(&self, focused: bool) {
        *self.shared.shown.lock().unwrap() = Some((Instant::now(), focused));
    }

    /// Native keystrokes and pastes are authoritative; a plain Enter clears the pending draft.
    pub fn record_input(&self, at: Instant, submitted: bool) {
        self.shared.input.lock().unwrap().record(at, submitted);
    }

    pub fn last_typed(&self) -> Option<Instant> {
        self.shared.input.lock().unwrap().last_typed
    }

    pub fn input_empty_since(&self, turn_ended: Option<Instant>) -> bool {
        if let Some(empty) = self.shared.input.lock().unwrap().empty_since(turn_ended) {
            return empty;
        }
        // Before native input is observed, ignore suggestions rendered with styling or a custom color.
        let term = self.term.lock();
        let grid = term.grid();
        let lines: Vec<String> = (0..grid.screen_lines())
            .map(|r| {
                let row = &grid[Line(r as i32)];
                let first = (0..grid.columns()).find(|&c| {
                    let ch = row[Column(c)].c;
                    ch != '\0' && !ch.is_whitespace()
                });
                (0..grid.columns())
                    .filter(|&c| !row[Column(c)].flags.contains(Flags::WIDE_CHAR_SPACER))
                    .map(|c| {
                        let cell = &row[Column(c)];
                        let marker = Some(c) == first && matches!(cell.c, '❯' | '›' | '>' | '─' | '━' | '═' | '╰');
                        let styled = cell.fg != Color::Named(NamedColor::Foreground)
                            || cell.flags.intersects(
                                Flags::DIM
                                    | Flags::BOLD_ITALIC
                                    | Flags::ALL_UNDERLINES
                                    | Flags::INVERSE
                                    | Flags::HIDDEN
                                    | Flags::STRIKEOUT,
                            );
                        if cell.c == '\0' || styled && !marker { ' ' } else { cell.c }
                    })
                    .collect()
            })
            .collect();
        let Some((index, prompt)) = lines
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, line)| line.trim_start().strip_prefix(['❯', '›', '>']).map(|prompt| (i, prompt)))
        else {
            return true;
        };
        prompt.trim().is_empty()
            && lines[index + 1..]
                .iter()
                .take_while(|line| !line.trim_start().starts_with(['─', '━', '═', '╰']))
                .all(|line| line.trim().is_empty())
    }

    /// Output cadence is a host decision informed by native-client visibility.
    /// These hints expire by wall clock, independently of egui frame counters.
    pub fn repaint_delay(&self) -> Duration {
        let shown = *self.shared.shown.lock().unwrap();
        let visible = shown.is_some_and(|(at, _)| at.elapsed() < Duration::from_millis(500));
        let focused = visible && shown.is_some_and(|(_, focused)| focused);
        if focused && self.last_typed().is_some_and(|at| at.elapsed() < Duration::from_secs(1)) {
            Duration::ZERO
        } else if focused {
            Duration::from_millis(16)
        } else if visible {
            Duration::from_millis(33)
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
            submitted: Mutex::new(None),
            exited: AtomicBool::new(false),
            bell: AtomicBool::new(false),
            changed: AtomicBool::new(false),
            revision: AtomicU64::new(0),
            clipboard: Mutex::new(None),
            wake,
            size: Mutex::new(size),
            shown: Mutex::new(None),
            input: Mutex::new(InputHistory::default()),
            colors: Mutex::new(colors),
        });
        let listener = Listener { sender: Arc::new(OnceLock::new()), shared: shared.clone() };
        let config = TermConfig { scrolling_history: scrollback, ..TermConfig::default() };
        let term = Arc::new(FairMutex::new(Term::new(config, &TermSize::new(80, 24), listener.clone())));
        let event_loop = EventLoop::new(term.clone(), listener.clone(), pty, true, false)?;
        let sender = event_loop.channel();
        let _ = listener.sender.set(sender.clone());
        event_loop.spawn();
        Ok(Terminal { process_id, stopped: AtomicBool::new(false), term, sender, shared })
    }

    /// Colors answered to later color queries, after a theme change.
    pub fn set_colors(&self, colors: Vec<Rgb>) {
        *self.shared.colors.lock().unwrap() = colors;
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
        self.write(atomics_core::steer::paste_bytes(text, bracketed));
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
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        // alacritty creates a new session/process group with setsid before exec.
        // Stop it before taking the descendant snapshot so the leader cannot spawn more work.
        // Interactive shells can give jobs their own groups; those must go away too.
        let leader = self.process_id as i32;
        unsafe {
            libc::kill(-leader, libc::SIGSTOP);
        }
        if !self.exited()
            && let Ok(output) = std::process::Command::new("/bin/ps").args(["-axo", "pid=,ppid=,pgid="]).output()
            && output.status.success()
        {
            let rows: Vec<Vec<i32>> = String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(|line| line.split_whitespace().filter_map(|part| part.parse().ok()).collect())
                .filter(|row: &Vec<i32>| row.len() == 3)
                .collect();
            let mut owned = std::collections::HashSet::from([leader]);
            loop {
                let before = owned.len();
                for row in &rows {
                    if owned.contains(&row[1]) {
                        owned.insert(row[0]);
                    }
                }
                if before == owned.len() {
                    break;
                }
            }
            let our_group = unsafe { libc::getpgrp() };
            for row in rows.iter().filter(|row| owned.contains(&row[0]) && row[2] > 1 && row[2] != our_group) {
                unsafe {
                    libc::kill(-row[2], libc::SIGKILL);
                }
            }
        }
        unsafe {
            libc::kill(-leader, libc::SIGKILL);
        }
        let _ = self.sender.send(Msg::Shutdown);
    }

    pub fn resize(&self, size: WindowSize) {
        let mut term = self.term.lock();
        term.resize(TermSize::new(size.num_cols as usize, size.num_lines as usize));
        self.shared.revision.fetch_add(1, Ordering::Release);
        drop(term);
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
    fn chief_input_history_clears_only_at_submit_or_turn_end() {
        let now = Instant::now();
        let mut input = InputHistory::default();
        assert_eq!(input.empty_since(Some(now)), None);
        input.record(now + Duration::from_secs(1), false);
        assert_eq!(input.empty_since(Some(now)), Some(false));
        input.record(now + Duration::from_secs(2), true);
        assert_eq!(input.empty_since(Some(now)), Some(true));
        input.record(now + Duration::from_secs(3), false);
        assert_eq!(input.empty_since(Some(now)), Some(false));
        assert_eq!(input.empty_since(Some(now + Duration::from_secs(4))), Some(true));
    }

    #[test]
    fn closing_or_replacing_terminal_kills_its_process_group() {
        for job_control in [false, true] {
            let dir = crate::test_support::tempdir().unwrap();
            let spawn = || {
                Terminal::spawn(
                    crate::host::Wake::default(),
                    Vec::new(),
                    "/bin/sh".into(),
                    vec![
                        if job_control { "-imc" } else { "-c" }.into(),
                        "trap '' HUP TERM; sleep 60 & echo $! > child; wait".into(),
                    ],
                    dir.path(),
                    HashMap::new(),
                    100,
                )
                .unwrap()
            };
            let mut terminal = Some(spawn());
            for replace in [true, false] {
                let pid = terminal.as_ref().unwrap().process_id() as i32;
                let deadline = Instant::now() + Duration::from_secs(5);
                let path = dir.path().join("child");
                while !path.exists() {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(5));
                }
                let child: i32 = std::fs::read_to_string(&path).unwrap().trim().parse().unwrap();
                assert_eq!(unsafe { libc::getpgid(child) } == pid, !job_control);
                std::fs::remove_file(path).unwrap();
                terminal = replace.then(spawn);
                let deadline = Instant::now() + Duration::from_secs(5);
                while unsafe { libc::kill(-pid, 0) == 0 || libc::kill(child, 0) == 0 } {
                    assert!(Instant::now() < deadline, "process group {pid} survived closing");
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert_eq!(unsafe { libc::kill(child, 0) }, -1, "worker survived");
            }
        }
    }

    #[test]
    fn listener_exit_wakes_the_host_without_a_client() {
        let wake = crate::host::Wake::default();
        let shared = Arc::new(Shared {
            title: Mutex::new(None),
            submitted: Mutex::new(None),
            exited: AtomicBool::new(false),
            bell: AtomicBool::new(false),
            changed: AtomicBool::new(false),
            revision: AtomicU64::new(0),
            clipboard: Mutex::new(None),
            wake: wake.clone(),
            size: Mutex::new(WindowSize { num_lines: 24, num_cols: 80, cell_width: 8, cell_height: 16 }),
            shown: Mutex::new(None),
            input: Mutex::new(InputHistory::default()),
            colors: Mutex::new(Vec::new()),
        });
        let listener = Listener { sender: Arc::new(OnceLock::new()), shared: shared.clone() };
        listener.send_event(TermEvent::Exit);
        let now = Instant::now();
        wake.wait_until(now + Duration::from_secs(30));
        assert!(now.elapsed() < Duration::from_secs(1));
        assert!(shared.exited.load(Ordering::Acquire));
        assert!(shared.changed.load(Ordering::Acquire));
    }
}
