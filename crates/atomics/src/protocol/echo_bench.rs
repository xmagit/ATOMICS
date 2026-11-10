//! Keystroke echo over `atomics.v1` on loopback, to compare with the desktop's 4.7 ms p50 / 12.3 ms
//! p95 keystroke-to-echo (docs/decisions/2026-11-02-vm-display.md). Ignored by default; run it in
//! release, as that measurement was:
//! `cargo test --release -p atomics --bin atomics protocol::echo_bench -- --ignored --nocapture`.
//!
//! A real PTY runs `cat` in the terminal's default (cooked) mode, so the kernel echoes each key as
//! it arrives. A client on a real loopback socket with `TCP_NODELAY` holds the lease, types one key
//! per frame and times from just before the frame is sent until its screen mirror shows the echo.
//! The server runs with its production settings (screens copied every 100 ms when focused, client
//! threads reading every 25 ms), so what is measured is the echo path, not a test configuration.
//! Keys come either while typing (60 ms apart, inside the echo window) or cold (400 ms apart, so
//! every thread involved has gone back to its slow cadence). For comparison the same keys are
//! written straight to the PTY and timed until the emulator shows them: the host's own echo time,
//! with no protocol at all.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use atomics_core::lease::Typist;
use atomics_core::projection::Projections;
use atomics_core::screen::{Screen, ScreenMirror};
use atomics_core::wire::{CAPABILITIES, Envelope, Message, Topic, VERSION};

use super::tests::TestClient;
use super::{ScreenSource, Server, ServerConfig, TerminalEntry, Terminals};
use crate::terminal::Terminal;
use crate::terminal::screen::Tracker;

struct NoDiffs;

impl super::DiffSource for NoDiffs {
    fn file(&self, _: &str, _: &str) -> Option<(u64, atomics_core::diff::FileDiff)> {
        None
    }
}

fn cat() -> (Terminal, crate::test_support::TempDir) {
    let dir = crate::test_support::tempdir().unwrap();
    let theme = crate::theme::Theme::from_config(&atomics_core::config::ThemeConfig::default());
    let terminal = Terminal::spawn(
        crate::host::Wake::default(),
        theme.terminal_colors(),
        crate::test_support::program("cat"),
        Vec::new(),
        dir.path(),
        HashMap::new(),
        1000,
    )
    .unwrap();
    (terminal, dir)
}

fn percentiles(mut times: Vec<Duration>) -> String {
    times.sort();
    let at = |q: f64| times[((times.len() as f64 - 1.0) * q).round() as usize].as_secs_f64() * 1000.0;
    let mean = times.iter().sum::<Duration>().as_secs_f64() * 1000.0 / times.len() as f64;
    format!(
        "n={} mean={mean:.2}ms p50={:.2}ms p90={:.2}ms p95={:.2}ms p99={:.2}ms max={:.2}ms",
        times.len(),
        at(0.5),
        at(0.9),
        at(0.95),
        at(0.99),
        at(1.0)
    )
}

/// Whether the screen shows `key` just left of the cursor, at column `col`.
fn echoed(screen: &Screen, col: u16, key: char) -> bool {
    let row = usize::from(screen.cursor.row);
    screen.cursor.col == col + 1
        && screen.lines.get(row).and_then(|l| l.cells.get(usize::from(col))).is_some_and(|c| c.c == key)
}

struct Remote {
    client: TestClient,
    mirror: ScreenMirror,
    typist: Typist,
    /// What is typed on the current line so far.
    line: String,
}

impl Remote {
    fn step(&mut self) {
        let Some(envelope) = self.client.next() else {
            let screen = self.mirror.screen.as_ref();
            panic!("no frame: {:?} {:?}", screen.map(|s| s.cursor), screen.map(Screen::text));
        };
        match envelope.message {
            Message::ScreenSnapshot(s) => drop(self.mirror.apply_snapshot(&s).expect("a valid snapshot")),
            Message::ScreenDelta(d) => {
                if self.mirror.apply_delta(&d).is_err() {
                    let resync = Message::Resync { reason: None, screens: vec!["s".into()] };
                    self.client.send(&Envelope::new(resync));
                }
            }
            Message::LeaseState(state) => self.typist.apply(&state),
            Message::Error { code, message } => panic!("{code:?}: {message}"),
            _ => {}
        }
    }

    fn until(&mut self, done: impl Fn(&Screen) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.mirror.screen.as_ref().is_some_and(&done) {
            assert!(Instant::now() < deadline, "the echo never came");
            self.step();
        }
    }

    fn type_key(&mut self, data: &str) {
        for frame in self.typist.input(data) {
            self.client.send(&Envelope::new(frame));
        }
    }

    /// Type keys `gap` apart and time each echo; Enter (untimed) every 60 columns.
    fn series(&mut self, keys: usize, gap: Duration) -> Vec<Duration> {
        let mut times = Vec::with_capacity(keys);
        for i in 0..keys {
            if self.line.len() == 60 {
                self.type_key("\r");
                let typed = std::mem::take(&mut self.line);
                // The kernel echoes the newline, then cat writes the line back.
                self.until(|s| {
                    let row = usize::from(s.cursor.row);
                    s.cursor.col == 0 && row > 0 && s.lines[row - 1].text() == typed
                });
            }
            std::thread::sleep(gap);
            let key = char::from(b'a' + (i % 26) as u8);
            let col = self.line.len() as u16;
            let started = Instant::now();
            self.type_key(&key.to_string());
            self.until(|s| echoed(s, col, key));
            times.push(started.elapsed());
            self.line.push(key);
        }
        times
    }
}

/// The host's own echo time: write to the PTY, wait for the emulator to show it.
fn native(terminal: &Terminal, keys: usize) -> Vec<Duration> {
    let handle = terminal.screen_handle();
    let (mut tracker, generations) = (Tracker::default(), AtomicU64::new(0));
    let mut times = Vec::with_capacity(keys);
    // Start on a fresh line, whatever was typed before.
    terminal.write(b"\r".to_vec());
    std::thread::sleep(Duration::from_millis(100));
    let mut line = 0u16;
    for i in 0..keys {
        if line == 60 {
            terminal.write(b"\r".to_vec());
            std::thread::sleep(Duration::from_millis(50));
            line = 0;
        }
        std::thread::sleep(Duration::from_millis(20));
        let key = char::from(b'A' + (i % 26) as u8);
        let before = handle.changes();
        let started = Instant::now();
        terminal.write(vec![key as u8]);
        loop {
            // Look only once the PTY thread says the screen changed, so the lock is left to it.
            if handle.changes() != before && echoed(&handle.capture(&mut tracker, &generations), line, key) {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "the PTY never echoed");
            std::hint::spin_loop();
        }
        times.push(started.elapsed());
        line += 1;
    }
    times
}

#[test]
#[ignore = "a measurement, not a check; run with --release --ignored --nocapture"]
fn keystroke_echo_latency() {
    let (terminal, _dir) = cat();
    let terminals = Arc::new(Terminals::default());
    let handle = Arc::new(terminal.screen_handle());
    terminals.publish(vec![TerminalEntry {
        session: "s".into(),
        identity: terminal.screen_identity(),
        source: handle.clone(),
        control: handle,
    }]);
    let config = ServerConfig::default();
    println!(
        "server: focused cadence {:?}, client tick {:?}, poll {:?}, echo window {:?}",
        config.screens.focused, config.session.tick, config.poll_interval, config.screens.echo_window
    );
    let server = Server::start(
        config,
        Arc::new(Projections::default()),
        Arc::new(NoDiffs),
        terminals,
        Arc::new(super::NoCommands),
    )
    .unwrap();

    let mut client = TestClient::connect(server.addr(), server.token());
    client.ws.get_ref().set_nodelay(true).unwrap();
    client.send(&Envelope::new(Message::Hello {
        client_id: "bench".into(),
        versions: vec![VERSION],
        capabilities: CAPABILITIES.iter().map(|c| c.to_string()).collect(),
        prior_epoch: None,
        prior_cursor: None,
    }));
    client.send(&Envelope::new(Message::Subscribe {
        topics: vec![Topic::Screen { session: "s".into(), focused: true }],
    }));
    let mut remote = Remote { client, mirror: ScreenMirror::default(), typist: Typist::new("s"), line: String::new() };
    let acquire = remote.typist.acquire(false);
    remote.client.send(&Envelope::new(acquire));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(remote.typist.may_type() && remote.mirror.screen.is_some()) {
        assert!(Instant::now() < deadline, "no lease or screen");
        remote.step();
    }

    let _ = remote.series(20, Duration::from_millis(60)); // warm up
    let typing = remote.series(300, Duration::from_millis(60));
    let cold = remote.series(100, Duration::from_millis(400));
    drop(remote);
    let host = native(&terminal, 300);
    println!("echo remote-typing {}", percentiles(typing));
    println!("echo remote-cold   {}", percentiles(cold));
    println!("echo host-pty      {}", percentiles(host));
    println!("echo desktop (vm-display.md, in-app, frame cadence included): p50=4.7ms p95=12.3ms");
    terminal.shutdown();
}
