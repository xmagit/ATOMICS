//! Host-owned PTY, child and emulator; notifications never depend on egui.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{ClipboardType, Config as TermConfig, Osc52, Term, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use anyhow::Result;
use atomics_core::lease::GridSize;

use crate::protocol::{Lease, TerminalControl};

const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
const CLIPBOARD_INTERVAL: Duration = Duration::from_secs(1);

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

#[derive(Default)]
struct Clipboard {
    enabled: bool,
    text: Option<String>,
    last_write: Option<Instant>,
}

impl Clipboard {
    fn store(&mut self, text: String, now: Instant) -> bool {
        if !self.enabled
            || text.len() > MAX_CLIPBOARD_BYTES
            || self.last_write.is_some_and(|last| now.saturating_duration_since(last) < CLIPBOARD_INTERVAL)
        {
            return false;
        }
        self.text = Some(text);
        self.last_write = Some(now);
        true
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
    // Only a desktop that focuses this terminal can enable OSC 52. Headless and remote
    // terminals start disabled; rejected writes are never saved for a later focus change.
    clipboard: Mutex<Clipboard>,
    wake: crate::host::Wake,
    size: Mutex<WindowSize>,
    shown: Mutex<Option<(Instant, bool)>>,
    input: Mutex<InputHistory>,
    /// The theme's 256 colors, foreground, background and cursor, answered to OSC 4/10/11/12.
    colors: Mutex<Vec<Rgb>>,
    /// The program's current title. `title` above is taken by the host; this one is only read.
    screen_title: Mutex<Option<String>>,
    /// Moves with title and theme changes, which do not move `revision`, so a remote screen
    /// stream knows to copy again. Nothing on the desktop reads it.
    screen_changes: AtomicU64,
    /// Which remote client may type into and resize this terminal. Nobody holds it unless the
    /// protocol server runs and a client asked; until then the desktop never takes its lock to type.
    lease: Lease,
    /// The terminal's own size: the desktop's pane, on a host with no desktop a client's pane, or
    /// the size the terminal started with. `size` is what is in force, which is a lease holder's
    /// size while it has one.
    local: Mutex<WindowSize>,
    /// Bytes lease holders sent that the PTY has not taken yet ([`CountedPty`]). A program that
    /// stops reading its input leaves them queued; the lease refuses more above a bound.
    unwritten: AtomicUsize,
}

impl Shared {
    fn new(wake: crate::host::Wake, size: WindowSize, colors: Vec<Rgb>) -> Self {
        Self {
            title: Mutex::new(None),
            submitted: Mutex::new(None),
            exited: AtomicBool::new(false),
            bell: AtomicBool::new(false),
            changed: AtomicBool::new(false),
            revision: AtomicU64::new(0),
            clipboard: Mutex::new(Clipboard::default()),
            wake,
            size: Mutex::new(size),
            shown: Mutex::new(None),
            input: Mutex::new(InputHistory::default()),
            colors: Mutex::new(colors),
            screen_title: Mutex::new(None),
            screen_changes: AtomicU64::new(0),
            lease: Lease::default(),
            local: Mutex::new(size),
            unwritten: AtomicUsize::new(0),
        }
    }

    fn set_title(&self, title: Option<String>) {
        // Remote screens carry at most MAX_TITLE_BYTES of it; a program may set any length.
        *self.screen_title.lock().unwrap() = title.as_deref().map(|t| atomics_core::screen::clip_title(t).to_string());
        self.screen_changes.fetch_add(1, Ordering::Release);
        *self.title.lock().unwrap() = title;
    }

    /// Update permission and drain the pending write atomically. Losing focus or disabling
    /// clipboard access drops a queued write too, even if it arrived while access was allowed.
    fn take_clipboard(&self, enabled: bool) -> Option<String> {
        let mut clipboard = self.clipboard.lock().unwrap();
        clipboard.enabled = enabled;
        clipboard.text.take().filter(|_| enabled)
    }
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
            TermEvent::Title(title) => self.shared.set_title(Some(title)),
            TermEvent::ResetTitle => self.shared.set_title(None),
            TermEvent::PtyWrite(text) => write(text),
            TermEvent::ColorRequest(index, format) => {
                let colors = self.shared.colors.lock().unwrap();
                if let Some(color) = colors.get(index).or(colors.get(256)) {
                    write(format(*color));
                }
            }
            TermEvent::TextAreaSizeRequest(format) => write(format(*self.shared.size.lock().unwrap())),
            TermEvent::ClipboardStore(ClipboardType::Clipboard, text) => {
                if !self.shared.clipboard.lock().unwrap().store(text, Instant::now()) {
                    return;
                }
            }
            // OSC 52 primary/secondary selection is not the host's CLIPBOARD.
            TermEvent::ClipboardStore(ClipboardType::Selection, _) => return,
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
    options: Mutex<TermConfig>,
    sender: EventLoopSender,
    pub shared: Arc<Shared>,
    /// The PTY's own end, to ask who holds the terminal.
    master: std::fs::File,
}

/// Remove instance routing from every terminal, including inherited bundle settings, then route it to the
/// host that runs it: `ATOMICS_SOCKET` names this host's own `socket`, so `atomics queue`, `atomics ctl` and
/// hooks started there reach this host whatever a login profile does (NixOS sets its session variables once
/// per login, and the host's own login shell already did). [`HOST_SOCKET_ENV`] marks it as the host's, so an
/// explicit `ATOMICS_HOME` there is still an instance of its own. A session's own hook pane is set again
/// after the inherited one is removed.
///
/// [`HOST_SOCKET_ENV`]: atomics_core::paths::HOST_SOCKET_ENV
pub(super) fn isolated_args(
    program: String,
    args: Vec<String>,
    socket: Option<&str>,
    hook_pane: Option<String>,
) -> Vec<String> {
    [
        "-u",
        "ATOMICS_SOCKET",
        "-u",
        atomics_core::paths::HOST_SOCKET_ENV,
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
        "-u",
        atomics_core::hook::HOOK_PANE_ENV,
    ]
    .into_iter()
    .map(str::to_string)
    .chain(socket.into_iter().flat_map(|socket| {
        [format!("ATOMICS_SOCKET={socket}"), format!("{}={socket}", atomics_core::paths::HOST_SOCKET_ENV)]
    }))
    .chain(hook_pane.map(|pane| format!("{}={pane}", atomics_core::hook::HOOK_PANE_ENV)))
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
        mut env: HashMap<String, String>,
        scrollback: usize,
    ) -> Result<Terminal> {
        let hook_pane = env.remove(atomics_core::hook::HOOK_PANE_ENV);
        // This process is the host whose terminal this is: its socket is the one to reach.
        let socket = atomics_core::paths::socket_path();
        let args = isolated_args(program, args, socket.to_str(), hook_pane);
        let options = tty::Options {
            shell: Some(tty::Shell::new("/usr/bin/env".into(), args)),
            working_directory: Some(cwd.to_path_buf()),
            drain_on_exit: true,
            env,
        };
        let size = WindowSize { num_lines: 24, num_cols: 80, cell_width: 8, cell_height: 16 };
        let pty = tty::new(&options, size, 0)?;
        let process_id = pty.child().id();
        let master = pty.file().try_clone()?;
        let shared = Arc::new(Shared::new(wake, size, colors));
        let pty = CountedPty::new(pty, shared.clone())?;
        let listener = Listener { sender: Arc::new(OnceLock::new()), shared: shared.clone() };
        // Start disabled, including before a sandbox or headless session produces its first byte.
        let config = TermConfig { scrolling_history: scrollback, osc52: Osc52::Disabled, ..TermConfig::default() };
        let term = Arc::new(FairMutex::new(Term::new(config.clone(), &TermSize::new(80, 24), listener.clone())));
        let event_loop = EventLoop::new(term.clone(), listener.clone(), pty, true, false)?;
        let sender = event_loop.channel();
        let _ = listener.sender.set(sender.clone());
        event_loop.spawn();
        Ok(Terminal {
            process_id,
            stopped: AtomicBool::new(false),
            term,
            options: Mutex::new(config),
            sender,
            shared,
            master,
        })
    }

    /// Keep the emulator from decoding OSC 52 at all while access is denied. Clipboard reads
    /// remain disabled even for a focused terminal that may copy.
    pub fn take_clipboard(&self, enabled: bool) -> Option<String> {
        let mut options = self.options.lock().unwrap();
        let osc52 = if enabled { Osc52::OnlyCopy } else { Osc52::Disabled };
        if options.osc52 != osc52 {
            options.osc52 = osc52;
            self.term.lock().set_options(options.clone());
        }
        self.shared.take_clipboard(enabled)
    }

    /// Colors answered to later color queries, after a theme change.
    pub fn set_colors(&self, colors: Vec<Rgb>) {
        *self.shared.colors.lock().unwrap() = colors;
        self.shared.screen_changes.fetch_add(1, Ordering::Release);
    }

    /// The identity of this terminal's screen handles (see `ScreenHandle::identity`).
    pub fn screen_identity(&self) -> usize {
        Arc::as_ptr(&self.shared) as usize
    }

    /// A handle the protocol uses to stream this terminal's screen and, for a lease holder, to
    /// type into it and resize it.
    pub fn screen_handle(&self) -> ScreenHandle {
        ScreenHandle { term: self.term.clone(), shared: self.shared.clone(), input: Input::Pty(self.sender.clone()) }
    }

    /// The remote client typing here, as its lease names it, and whether its size is in force; `None`
    /// when nobody holds the lease (always with remote access off). Lock-free then.
    pub fn remote_holder(&self) -> Option<(std::sync::Arc<str>, bool)> {
        self.shared.lease.holder()
    }

    /// Someone typed on the desktop: a remote client's lease on this terminal ends, and its size
    /// with it. Nothing happens (and no lock is taken) when nobody holds one, which is always so
    /// with remote access off.
    pub fn take_back(&self) {
        if self.shared.lease.held() {
            let handle = self.screen_handle();
            (&handle as &dyn TerminalControl).take_back();
        }
    }

    /// Lines kept above the screen; fewer drops the oldest at once.
    pub fn set_scrollback(&self, lines: usize) {
        let mut options = self.options.lock().unwrap();
        options.scrolling_history = lines;
        self.term.lock().set_options(options.clone());
    }

    pub fn exited(&self) -> bool {
        self.shared.exited.load(Ordering::Acquire)
    }

    /// Whether something other than the program started here holds the terminal: what an interactive shell runs,
    /// which gets a process group of its own. A program that runs things itself keeps the terminal, and what runs
    /// inside a sandbox looks the same from here, so neither shows.
    pub fn foreground_process(&self) -> bool {
        use std::os::fd::AsRawFd;
        // SAFETY: tcgetpgrp on a descriptor this terminal owns.
        let holder = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
        holder > 0 && holder != self.process_id as i32
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

    /// Types `text` into the program's prompt without pressing Enter.
    pub fn draft(&self, text: &str) {
        let bracketed = self.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        self.term.lock().scroll_display(Scroll::Bottom);
        self.write(atomics_core::steer::paste_bytes(text, bracketed));
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
            && let Some(rows) = process_groups()
        {
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

    /// The desktop's pane size. Applied at once, unless a remote lease holder asked for its own
    /// size; then it is applied when that lease ends.
    pub fn resize(&self, size: WindowSize) {
        let handle = self.screen_handle();
        (&handle as &dyn TerminalControl).local_resize(|| *self.shared.local.lock().unwrap() = size);
    }
}

/// System locations work with a minimal PATH. Profile fallbacks must be absolute,
/// so empty entries, `.` and relative folders cannot select a program in the working directory.
fn process_groups() -> Option<Vec<[i32; 3]>> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let candidates = ["/bin/ps", "/usr/bin/ps", "/run/current-system/sw/bin/ps"]
        .map(PathBuf::from)
        .into_iter()
        .chain(std::env::split_paths(&path).filter(|dir| dir.is_absolute()).map(|dir| dir.join("ps")));
    for program in candidates {
        let Ok(output) = std::process::Command::new(program).args(["-axo", "pid=,ppid=,pgid="]).output() else {
            continue;
        };
        if output.status.success() {
            return Some(
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter_map(|line| {
                        line.split_whitespace().filter_map(|part| part.parse().ok()).collect::<Vec<_>>().try_into().ok()
                    })
                    .collect(),
            );
        }
    }
    None
}

/// Resize the emulator and the PTY, and record the size answered to size queries.
fn apply_size(term: &FairMutex<Term<Listener>>, shared: &Shared, input: &Input, size: WindowSize) {
    let mut locked = term.lock();
    locked.resize(TermSize::new(size.num_cols as usize, size.num_lines as usize));
    shared.revision.fetch_add(1, Ordering::Release);
    drop(locked);
    *shared.size.lock().unwrap() = size;
    match input {
        Input::Pty(sender) => {
            let _ = sender.send(Msg::Resize(size));
        }
        #[cfg(test)]
        Input::Echo(_) => {}
    }
}

/// Where a lease holder's input goes.
#[derive(Clone)]
enum Input {
    Pty(EventLoopSender),
    /// A test emulator with no PTY: input is recorded and parsed as if the program echoed it.
    #[cfg(test)]
    Echo(Arc<Echo>),
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// alacritty's PTY with its writes counted, so a lease holder's input that the program has not
/// read yet is known ([`Shared::unwritten`]). Everything else is the PTY's own.
struct CountedPty {
    pty: tty::Pty,
    writer: CountedWriter,
}

impl CountedPty {
    fn new(pty: tty::Pty, shared: Arc<Shared>) -> std::io::Result<Self> {
        // The same open file, so the same non-blocking mode and the same order of bytes.
        let file = pty.file().try_clone()?;
        Ok(Self { pty, writer: CountedWriter { file, shared } })
    }
}

struct CountedWriter {
    file: std::fs::File,
    shared: Arc<Shared>,
}

impl std::io::Write for CountedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = std::io::Write::write(&mut self.file, bytes)?;
        // The desktop's own input is written here too but never counted: stop at zero.
        let unwritten = &self.shared.unwritten;
        let mut queued = unwritten.load(Ordering::Acquire);
        while queued > 0 {
            match unwritten.compare_exchange_weak(
                queued,
                queued.saturating_sub(written),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(now) => queued = now,
            }
        }
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(&mut self.file)
    }
}

impl tty::EventedReadWrite for CountedPty {
    type Reader = <tty::Pty as tty::EventedReadWrite>::Reader;
    type Writer = CountedWriter;

    unsafe fn register(
        &mut self,
        poll: &Arc<polling::Poller>,
        interest: polling::Event,
        mode: polling::PollMode,
    ) -> std::io::Result<()> {
        // SAFETY: the caller's contract is the PTY's; this registers exactly what it would.
        unsafe { self.pty.register(poll, interest, mode) }
    }

    fn reregister(
        &mut self,
        poll: &Arc<polling::Poller>,
        interest: polling::Event,
        mode: polling::PollMode,
    ) -> std::io::Result<()> {
        self.pty.reregister(poll, interest, mode)
    }

    fn deregister(&mut self, poll: &Arc<polling::Poller>) -> std::io::Result<()> {
        self.pty.deregister(poll)
    }

    fn reader(&mut self) -> &mut Self::Reader {
        self.pty.reader()
    }

    fn writer(&mut self) -> &mut CountedWriter {
        &mut self.writer
    }
}

impl tty::EventedPty for CountedPty {
    fn next_child_event(&mut self) -> Option<tty::ChildEvent> {
        self.pty.next_child_event()
    }
}

impl alacritty_terminal::event::OnResize for CountedPty {
    fn on_resize(&mut self, size: WindowSize) {
        self.pty.on_resize(size);
    }
}

/// What the protocol thread needs to stream one terminal: the emulator, locked only to copy the
/// screen, and the counters that say whether there is anything new to copy. Holding one does not
/// keep the PTY or child alive.
#[derive(Clone)]
pub struct ScreenHandle {
    term: Arc<FairMutex<Term<Listener>>>,
    shared: Arc<Shared>,
    input: Input,
}

impl ScreenHandle {
    /// The same for every handle to one terminal, different for its replacement.
    #[cfg(test)]
    pub fn identity(&self) -> usize {
        Arc::as_ptr(&self.shared) as usize
    }

    /// The title and theme are cloned before the emulator is locked: the listener takes their
    /// locks while the emulator is locked, so taking them inside would invert that order.
    fn inputs(&self) -> (Vec<Rgb>, Option<String>) {
        (self.shared.colors.lock().unwrap().clone(), self.shared.screen_title.lock().unwrap().clone())
    }
}

impl crate::protocol::ScreenSource for ScreenHandle {
    fn changes(&self) -> u64 {
        self.shared.revision.load(Ordering::Acquire).wrapping_add(self.shared.screen_changes.load(Ordering::Acquire))
    }

    fn capture(&self, tracker: &mut super::screen::Tracker, generations: &AtomicU64) -> atomics_core::screen::Screen {
        let (theme, title) = self.inputs();
        let term = self.term.lock();
        super::screen::capture(&term, &theme, title, tracker, generations)
    }

    fn history(
        &self,
        tracker: &mut super::screen::Tracker,
        generations: &AtomicU64,
        before: u64,
        count: u32,
    ) -> super::screen::HistoryLines {
        let term = self.term.lock();
        super::screen::history(&term, tracker, generations, before, count)
    }
}

impl TerminalControl for ScreenHandle {
    fn lease(&self) -> &Lease {
        &self.shared.lease
    }

    fn write(&self, bytes: Vec<u8>) {
        match &self.input {
            Input::Pty(sender) => {
                // Counted before it is queued, so the PTY's write can never be counted first.
                self.shared.unwritten.fetch_add(bytes.len(), Ordering::AcqRel);
                if sender.send(Msg::Input(Cow::Owned(bytes))).is_err() {
                    // The PTY is gone and will take nothing more.
                    self.shared.unwritten.store(0, Ordering::Release);
                }
            }
            #[cfg(test)]
            Input::Echo(echo) => {
                echo.written.lock().unwrap().extend_from_slice(&bytes);
                if echo.stalled.load(Ordering::Acquire) {
                    // A program that reads nothing: the bytes wait, unread.
                    self.shared.unwritten.fetch_add(bytes.len(), Ordering::AcqRel);
                } else {
                    echo.feed(&self.term, &self.shared, &bytes);
                }
            }
        }
    }

    fn unwritten(&self) -> usize {
        self.shared.unwritten.load(Ordering::Acquire)
    }

    fn resize(&self, size: Option<GridSize>) {
        let local = *self.shared.local.lock().unwrap();
        let size = size.map_or(local, |s| WindowSize {
            num_cols: s.cols,
            num_lines: s.rows,
            cell_width: s.cell_width.unwrap_or(local.cell_width),
            cell_height: s.cell_height.unwrap_or(local.cell_height),
        });
        apply_size(&self.term, &self.shared, &self.input, size);
    }

    fn set_own_size(&self, size: GridSize) {
        let mut local = self.shared.local.lock().unwrap();
        *local = WindowSize {
            num_cols: size.cols,
            num_lines: size.rows,
            cell_width: size.cell_width.unwrap_or(local.cell_width),
            cell_height: size.cell_height.unwrap_or(local.cell_height),
        };
    }
}

#[cfg(test)]
pub(crate) struct Echo {
    parser: Mutex<alacritty_terminal::vte::ansi::Processor>,
    written: Mutex<Vec<u8>>,
    /// The program reads nothing; input waits in `Shared::unwritten`.
    stalled: AtomicBool,
}

#[cfg(test)]
impl Echo {
    /// Parse output as the PTY thread would, and say the screen changed.
    fn feed(&self, term: &FairMutex<Term<Listener>>, shared: &Shared, bytes: &[u8]) {
        let mut term = term.lock();
        self.parser.lock().unwrap().advance(&mut *term, bytes);
        drop(term);
        shared.revision.fetch_add(1, Ordering::Release);
    }
}

/// An emulator without a PTY or child, fed by the test, behind a real `ScreenHandle`. Input a
/// lease holder sends is recorded and echoed onto the screen.
#[cfg(test)]
pub(crate) struct Detached {
    pub handle: ScreenHandle,
    echo: Arc<Echo>,
}

#[cfg(test)]
impl Detached {
    pub fn new(cols: usize, rows: usize, scrollback: usize) -> Self {
        let size = WindowSize { num_lines: rows as u16, num_cols: cols as u16, cell_width: 8, cell_height: 16 };
        let shared = Arc::new(Shared::new(crate::host::Wake::default(), size, Vec::new()));
        let listener = Listener { sender: Arc::new(OnceLock::new()), shared: shared.clone() };
        let config = TermConfig { scrolling_history: scrollback, ..TermConfig::default() };
        let term = Arc::new(FairMutex::new(Term::new(config, &TermSize::new(cols, rows), listener)));
        let echo = Arc::new(Echo {
            parser: Mutex::new(Default::default()),
            written: Mutex::new(Vec::new()),
            stalled: AtomicBool::new(false),
        });
        Self { handle: ScreenHandle { term, shared, input: Input::Echo(echo.clone()) }, echo }
    }

    /// Parse output as the PTY thread would, and say the screen changed.
    pub fn feed(&self, bytes: &[u8]) {
        self.echo.feed(&self.handle.term, &self.handle.shared, bytes);
    }

    /// Everything lease holders wrote, in order.
    pub fn written(&self) -> String {
        String::from_utf8_lossy(&self.echo.written.lock().unwrap()).into_owned()
    }

    /// The size in force, as the PTY would report it.
    pub fn size(&self) -> (u16, u16) {
        let size = *self.handle.shared.size.lock().unwrap();
        (size.num_cols, size.num_lines)
    }

    /// The desktop typed into this terminal.
    pub fn take_back(&self) -> bool {
        (&self.handle as &dyn TerminalControl).take_back()
    }

    /// Stop the program reading its input (`true`), or let it read again: what waited is taken
    /// (and not echoed).
    pub fn stall(&self, stalled: bool) {
        self.echo.stalled.store(stalled, Ordering::Release);
        if !stalled {
            self.handle.shared.unwritten.store(0, Ordering::Release);
        }
    }

    /// Say the screen changed without touching the emulator.
    pub fn touch(&self) {
        self.handle.shared.revision.fetch_add(1, Ordering::Release);
    }

    /// The desktop's pane changed size: applied as `Terminal::resize` applies it.
    pub fn resize(&self, cols: usize, rows: usize) {
        let size = WindowSize { num_lines: rows as u16, num_cols: cols as u16, cell_width: 8, cell_height: 16 };
        (&self.handle as &dyn TerminalControl).local_resize(|| *self.handle.shared.local.lock().unwrap() = size);
    }

    /// Hold the emulator lock, as a busy PTY thread or a painting frame does.
    pub fn lock(&self) -> impl Drop + '_ {
        self.handle.term.lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_counted_writer_takes_what_the_pty_wrote_off_the_unwritten_input() {
        let size = WindowSize { num_lines: 24, num_cols: 80, cell_width: 8, cell_height: 16 };
        let shared = Arc::new(Shared::new(crate::host::Wake::default(), size, Vec::new()));
        let dir = tempfile::tempdir().unwrap();
        let file = std::fs::File::create(dir.path().join("pty")).unwrap();
        let mut writer = CountedWriter { file, shared: shared.clone() };
        shared.unwritten.store(5, Ordering::Release);
        assert_eq!(std::io::Write::write(&mut writer, b"abc").unwrap(), 3);
        assert_eq!(shared.unwritten.load(Ordering::Acquire), 2);
        // The desktop's own input is written too, and never counted below zero.
        std::io::Write::write_all(&mut writer, b"desktop").unwrap();
        assert_eq!(shared.unwritten.load(Ordering::Acquire), 0);
    }

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
        const CHILD: &str = "ATOMICS_TERMINAL_CLEANUP_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let dir = crate::test_support::tempdir().unwrap();
            let bin = dir.path().join("bin");
            crate::test_support::link_tools(&bin, &["bash", "sleep", "sh", "cat"]);
            crate::test_support::write_program(dir.path().join("ps"), "#!/bin/sh\n: > ps-ran\nexit 1\n");
            let path = std::env::join_paths([Path::new(""), Path::new("."), Path::new("relative"), &bin]).unwrap();
            let output = crate::test_support::output(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "terminal::runtime::tests::closing_or_replacing_terminal_kills_its_process_group",
                        "--nocapture",
                    ])
                    .current_dir(dir.path())
                    .env(CHILD, "1")
                    .env("PATH", path),
            );
            assert!(output.status.success(), "{output:?}");
            assert!(!dir.path().join("ps-ran").exists(), "cleanup executed ps from the working directory");
            return;
        }
        for job_control in [false, true] {
            let dir = crate::test_support::tempdir().unwrap();
            let spawn = || {
                Terminal::spawn(
                    crate::host::Wake::default(),
                    Vec::new(),
                    crate::test_support::program("bash"),
                    vec![
                        "--noprofile".into(),
                        "--norc".into(),
                        if job_control { "-imc" } else { "-c" }.into(),
                        "trap '' HUP TERM; sleep 60 & echo $! > child; wait".into(),
                    ],
                    dir.path(),
                    [
                        ("HOME".into(), dir.path().display().to_string()),
                        ("ENV".into(), "/dev/null".into()),
                        ("BASH_ENV".into(), "/dev/null".into()),
                    ]
                    .into(),
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
        let size = WindowSize { num_lines: 24, num_cols: 80, cell_width: 8, cell_height: 16 };
        let shared = Arc::new(Shared::new(wake.clone(), size, Vec::new()));
        let listener = Listener { sender: Arc::new(OnceLock::new()), shared: shared.clone() };
        listener.send_event(TermEvent::Exit);
        let now = Instant::now();
        wake.wait_until(now + Duration::from_secs(30));
        assert!(now.elapsed() < Duration::from_secs(1));
        assert!(shared.exited.load(Ordering::Acquire));
        assert!(shared.changed.load(Ordering::Acquire));
    }

    #[test]
    fn osc52_discards_denied_writes_and_pending_writes_when_focus_is_lost() {
        let terminal = Detached::new(80, 24, 100);
        let shared = &terminal.handle.shared;
        let copy = b"\x1b]52;c;Y2xpcGJvYXJk\x07";
        terminal.feed(copy);
        assert_eq!(shared.take_clipboard(true), None, "disabled writes cannot be replayed on focus");
        terminal.feed(copy);
        assert_eq!(shared.take_clipboard(true).as_deref(), Some("clipboard"));
        shared.clipboard.lock().unwrap().last_write = Some(Instant::now() - CLIPBOARD_INTERVAL);
        terminal.feed(copy);
        assert_eq!(shared.take_clipboard(false), None, "losing focus discards a pending write");
        terminal.feed(copy);
        assert_eq!(shared.take_clipboard(true), None, "background writes are not buffered");
        shared.clipboard.lock().unwrap().last_write = Some(Instant::now() - CLIPBOARD_INTERVAL);
        terminal.feed(copy);
        assert_eq!(shared.take_clipboard(true).as_deref(), Some("clipboard"));
    }

    #[test]
    fn osc52_caps_decoded_bytes_and_rate_without_delaying_the_next_valid_copy() {
        let now = Instant::now();
        let mut clipboard = Clipboard { enabled: true, ..Clipboard::default() };
        assert!(!clipboard.store("x".repeat(MAX_CLIPBOARD_BYTES + 1), now));
        assert!(clipboard.text.is_none());
        assert!(clipboard.store("x".repeat(MAX_CLIPBOARD_BYTES), now));
        assert_eq!(clipboard.text.take().unwrap().len(), MAX_CLIPBOARD_BYTES);
        for millis in [1, 16, 500, 999] {
            assert!(!clipboard.store("burst".into(), now + Duration::from_millis(millis)));
        }
        assert!(clipboard.text.is_none(), "rejected bursts never overwrite the clipboard");
        assert!(clipboard.store("next".into(), now + CLIPBOARD_INTERVAL));
        assert_eq!(clipboard.text.as_deref(), Some("next"));
    }

    #[test]
    fn osc52_primary_and_secondary_selections_do_not_overwrite_the_clipboard() {
        let terminal = Detached::new(80, 24, 100);
        let shared = &terminal.handle.shared;
        shared.take_clipboard(true);
        for selection in ['p', 's'] {
            terminal.feed(format!("\x1b]52;{selection};c2VsZWN0aW9u\x07").as_bytes());
            assert_eq!(shared.take_clipboard(true), None, "selection {selection}");
        }
        terminal.feed(b"\x1b]52;c;Y2xpcGJvYXJk\x07");
        assert_eq!(shared.take_clipboard(true).as_deref(), Some("clipboard"));
    }

    #[test]
    fn osc52_decoding_stays_disabled_when_denied_and_scrollback_changes() {
        use alacritty_terminal::vte::ansi::Handler;
        let dir = crate::test_support::tempdir().unwrap();
        let terminal = Terminal::spawn(
            crate::host::Wake::default(),
            Vec::new(),
            "/bin/sh".into(),
            vec!["-c".into(), "read -r line".into()],
            dir.path(),
            HashMap::new(),
            100,
        )
        .unwrap();
        let decoded = || {
            // Permit the listener independently so this checks the emulator's own denial.
            terminal.shared.take_clipboard(true);
            terminal.shared.clipboard.lock().unwrap().last_write = None;
            terminal.term.lock().clipboard_store(b'c', b"Y2xpcGJvYXJk");
            terminal.shared.take_clipboard(true)
        };
        assert!(decoded().is_none(), "disabled from the first byte, before the desktop ever focuses it");
        terminal.take_clipboard(true);
        assert_eq!(decoded().as_deref(), Some("clipboard"), "copy-only mode decodes when the desktop permits it");
        terminal.take_clipboard(false);
        assert!(decoded().is_none(), "denied writes never reach the event listener");
        terminal.set_scrollback(200);
        assert!(decoded().is_none(), "a config reload must not reset disabled OSC 52 to the default");
        terminal.shutdown();
    }

    #[test]
    fn osc52_queries_receive_no_reply_on_the_pty() {
        for osc52 in [Osc52::OnlyCopy, Osc52::CopyPaste] {
            let dir = crate::test_support::tempdir().unwrap();
            let terminal = Terminal::spawn(crate::host::Wake::default(), Vec::new(), "/bin/sh".into(), vec![
                "-c".into(),
                "stty raw -echo; printf ready; dd bs=1 count=1 of=/dev/null 2>/dev/null; printf '\\033]52;c;?\\007\\033]52;p;?\\007\\033[5n'; dd bs=1 count=4 of=reply 2>/dev/null; : > done; read -r line".into(),
            ], dir.path(), HashMap::new(), 100).unwrap();
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
            while !terminal.screen_text(24).contains("ready") {
                assert!(Instant::now() < deadline, "query fixture did not become ready");
                std::thread::sleep(Duration::from_millis(5));
            }
            terminal.take_clipboard(true);
            // CopyPaste also tests the listener's independent refusal of ClipboardLoad events.
            terminal.term.lock().set_options(TermConfig { scrolling_history: 100, osc52, ..TermConfig::default() });
            terminal.write(b"x".to_vec());
            while !dir.path().join("done").exists() {
                assert!(Instant::now() < deadline, "terminal never answered the status-query barrier");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                std::fs::read(dir.path().join("reply")).unwrap(),
                b"\x1b[0n",
                "only the status query gets a reply"
            );
            assert!(terminal.take_clipboard(true).is_none());
            terminal.shutdown();
        }
    }
}
