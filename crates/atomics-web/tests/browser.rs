//! The real bundle in headless Chrome, against the fake host of the unit tests behind a fake broker. Both
//! tests need the built bundle and Chrome, so they are ignored by default:
//!
//!     scripts/build-web.sh
//!     cargo test -p atomics-web --test browser -- --ignored
//!
//! `ATOMICS_WEB_DIST` names another bundle folder and `CHROME` another browser binary. The client's page is
//! served under the strict CSP the bundle is built for.
//!
//! `the_bundle_pairs_reconnects_and_reloads_in_a_browser`: a page on another origin frames the bundle and
//! boots it over `postMessage`, a third origin's frame keeps posting a boot of its own, and the browser pairs
//! (keeping its key in IndexedDB), says `hello`, reconnects with a fresh ticket and resumes, and after a
//! reload dials again with the key read back from IndexedDB, opening on an agent's terminal. There, through
//! DevTools: a click focuses the input overlay, which takes the lease; typed text and Return reach the host
//! and echo; a drag selects the echoed line from the client's own cells; the desktop takes the lease back,
//! which blurs the overlay; and a new click takes it again. `ATOMICS_WEB_SCREENSHOT=out.png` also saves what
//! the page shows once the board is in, and `ATOMICS_WEB_TERMINAL_SCREENSHOT=out.png` the terminal with its
//! echo selected.
//!
//! `the_bundle_works_on_a_phone_by_touch`: the same bundle on an iPhone (390 × 844 points at 3 pixels a
//! point, a touch screen, Safari's user agent), framed full screen by a hosting page, and used by touch alone
//! (`Input.dispatchTouchEvent`). It pairs, shows the board, the AGENTS list and a two-file diff, approves the
//! plan (`plan_command` approve) and a tool request with a tap on their buttons, and opens an agent's tile,
//! where a tap focuses the input overlay; text, Return, Backspace on an empty line and the key bar's ↑ (a key
//! 44 CSS pixels each way) reach the host as their bytes, and once the desktop takes the terminal back the
//! field has lost the focus and typing sends nothing. Then a long press on a word selects that word alone,
//! its end handle dragged over the next word takes it in, and Copy copies exactly those two words. Every
//! screen is saved as a PNG in `target/phone-shots/` (emptied first) for a person to look at. The client
//! draws on a canvas, so the test finds what to tap by colour in the screenshots, using the palette the
//! client draws with, and the pane bar from the client's own layout code, whose points are `layout::ZOOM`
//! CSS pixels.
//!
//! `the_bundle_draws_the_desktops_frame_in_a_wide_window`: the bundle in a 1600 × 1000 window draws the
//! desktop's own frame over the fake host: the top bar, the chief docked with its terminal, the agent's tile
//! and the plan's board. Its screenshot is saved as `target/wide-shots/browser.png`; where
//! `target/wide-shots/desktop.png` is (`scripts/wide-desktop-shot.sh` takes the desktop's QA screenshot of the
//! same board), the two go side by side into `target/wide-shots/side-by-side.png`. Then a click into the
//! agent's terminal types there, a click into the chief's moves the input there and lets go of the agent's
//! lease, and Approve plan approves.
//!
//! `the_input_gives_way_to_a_floating_window` and `a_menu_over_a_terminal_acts_and_takes_no_lease`: the input
//! lies over a terminal, and real clicks where something else lies over that terminal reach what the user sees.
//!
//! `a_page_of_many_plans_opens_on_the_open_plan` and `a_phone_of_many_plans_shows_the_open_plans_board`: a host
//! of 26 plans, 25 of them archived, opens with the open plan's Approve plan on screen, in a wide window and on
//! a phone, where History reaches the others. Their screenshots are saved in `target/many-plans-shots/`.
//!
//! The actions a browser offers, against the fake host, with real clicks and touches where the fake board puts
//! them. `text_boxes_take_the_keyboard_from_a_terminal`: while a terminal has the focus, a click on the steer bar
//! or a comment box moves the keyboard there; what is typed reaches the host as a steer or a comment, Send
//! review sends it, and nothing reaches the terminal, until a click back types there again. A refused steer says
//! why and comes back to the bar. `a_board_and_a_tile_act_in_a_wide_window`: start, Dispatch, and a tile's close
//! with its confirmation, which takes its part of the input. `a_plan_closes_and_a_new_session_starts`: the
//! picker of `+ New`, and Close plan… offering Archive alone. `a_row_steers_and_a_review_column_acts`:
//! "Include in selected" and a steer to the selected, then unblock, integrate, accept and Integrate in the
//! board's further columns. `a_phone_acts_by_touch`, at 360 CSS pixels: integrate, accept and Integrate on the
//! board, Close plan… with its confirmation, a comment and Send review after a terminal had the focus, a tile's
//! close, and the archived plan from History. `a_shell_under_another_name_gets_no_steer_for_want_of_all_agents`:
//! the target's list has no row that reaches a `zsh` session.
//! `the_keyboard_in_a_text_box_lets_go_of_the_terminals_lease`: a card's open types in its agent, the steer bar or a
//! comment box taking the keyboard releases the lease, and a click back asks again; Shift+Enter makes a new line.
//! `a_steer_one_target_missed_names_it_and_stays_out_of_the_bar` and
//! `a_note_takes_the_pointer_and_a_refused_comment_comes_back`: what a refusal leaves.
//! `a_comment_box_keeps_its_draft_while_the_connection_is_down`, `a_viewer_has_no_comment_actions_in_a_diff`,
//! `a_click_back_within_a_round_trip_types_at_once` and `a_finished_plan_picked_on_a_phone_shows_in_the_strip` say
//! what they do. Their screenshots are saved in
//! `target/actions-shots/`.

#[allow(dead_code)] // shared with the unit tests, which use more of it
#[path = "../src/fake_host.rs"]
mod fake_host;

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use atomics_core::approvals::Decision;
use atomics_core::ops::PlanAction;
use atomics_core::wire::{ErrorCode, Message, Topic, WireError};
use atomics_ui::theme::{Fonts, Palette};
use atomics_web::boot::{self, FromParent, ParentOrigin};
use atomics_web::layout::{self, Viewport};
use atomics_web::views::{self, Tab};
use egui::{Color32, Pos2, Rect, pos2, vec2};
use serde_json::json;
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::http::HeaderValue;
use tungstenite::protocol::CloseFrame;
use tungstenite::protocol::frame::coding::CloseCode;

use fake_host::{FakeHost, HOST_ID};

/// The longest any one step may take; a step that takes longer fails, naming what it waited for.
const WAIT: Duration = Duration::from_secs(30);
/// The longest one read from DevTools may block.
const READ: Duration = Duration::from_secs(5);
/// The longest the whole test may take: then it says what it saw, stops the browser and exits.
const OVERALL: Duration = Duration::from_secs(150);
const PHONE_OVERALL: Duration = Duration::from_secs(240);

/// The phone: an iPhone's screen in CSS pixels (egui's points), its pixels a point, and Safari's user agent.
const PHONE: (f32, f32) = (390.0, 844.0);
const PHONE_SCALE: f32 = 3.0;
const IOS_SAFARI: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_7 like Mac OS X) AppleWebKit/605.1.15 \
                          (KHTML, like Gecko) Version/26.0 Mobile/15E148 Safari/604.1";

/// What the servers saw, in order, with a way to wait for a line.
#[derive(Default)]
struct Log {
    lines: Mutex<Vec<String>>,
    changed: Condvar,
}

impl Log {
    fn push(&self, line: impl Into<String>) {
        self.lines.lock().unwrap().push(line.into());
        self.changed.notify_all();
    }

    /// Wait until `count` lines equal `line`.
    fn wait(&self, line: &str, count: usize) {
        let deadline = Instant::now() + WAIT;
        let mut lines = self.lines.lock().unwrap();
        while lines.iter().filter(|l| *l == line).count() < count {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "timed out waiting for {line:?} ×{count}; saw {:#?}", *lines);
            lines = self.changed.wait_timeout(lines, left).unwrap().0;
        }
    }

    fn has(&self, line: &str) -> bool {
        self.lines.lock().unwrap().iter().any(|l| l == line)
    }
}

/// The browser's console lines: what the page logged, and any CSP refusal.
fn console(log: &Path) {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    for line in text.lines().filter(|l| l.contains("CONSOLE") || l.contains("Content Security Policy")) {
        eprintln!("chrome: {line}");
    }
}

/// Kills the browser however the test ends, and shows the console of a failed run. Its profile goes after it.
struct Browser {
    child: Arc<Mutex<Child>>,
    log: PathBuf,
    profile: tempfile::TempDir,
}

impl Browser {
    /// Where Chrome writes `DevToolsActivePort`.
    fn devtools(&self) -> PathBuf {
        self.profile.path().join("profile")
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if thread::panicking() {
            console(&self.log);
        }
    }
}

/// Past `overall`, say what the servers saw and the browser logged, stop the browser and exit.
fn watchdog(child: Arc<Mutex<Child>>, seen: Arc<Log>, log: PathBuf, overall: Duration) {
    thread::spawn(move || {
        thread::sleep(overall);
        eprintln!("browser test: still running after {overall:?}; the servers saw {:#?}", seen.lines.lock().unwrap());
        console(&log);
        if let Ok(mut child) = child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        std::process::exit(1);
    });
}

struct Reply {
    kind: &'static str,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn new(kind: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self { kind, headers: Vec::new(), body: body.into() }
    }
}

type Handler = dyn Fn(&str, &str, &[u8]) -> Option<Reply> + Send + Sync;

/// A plain HTTP/1.1 server, one thread per connection.
fn serve(listener: TcpListener, handler: Arc<Handler>) {
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let handler = handler.clone();
            thread::spawn(move || {
                let _ = answer(stream, &*handler);
            });
        }
    });
}

fn answer(mut stream: TcpStream, handler: &Handler) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    let mut parts = first.split_whitespace();
    let (method, target) = (parts.next().unwrap_or_default().to_string(), parts.next().unwrap_or("/").to_string());
    let mut length = 0;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header == "\r\n" {
            break;
        }
        if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let path = target.split('?').next().unwrap_or("/");
    let (status, reply) = match handler(&method, path, &body) {
        Some(reply) => ("200 OK", reply),
        None => ("404 Not Found", Reply::new("text/plain", "not found")),
    };
    write!(stream, "HTTP/1.1 {status}\r\nContent-Type: {}\r\nContent-Length: {}\r\n", reply.kind, reply.body.len())?;
    write!(stream, "Cache-Control: no-store\r\nConnection: close\r\n")?;
    for (name, value) in &reply.headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    write!(stream, "\r\n")?;
    stream.write_all(&reply.body)?;
    stream.flush()
}

fn content_type(path: &str) -> &'static str {
    match Path::new(path).extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

/// Frames the host sends on its own (the desktop typing, say), which the broker passes on.
type Pending = Arc<Mutex<Vec<Vec<u8>>>>;

/// The broker's line to the browser, which a test can spoil.
#[derive(Default)]
struct Line {
    /// Set, the broker drops the channel.
    cut: AtomicBool,
    /// Set, the broker turns the browser away, as while the computer is off.
    down: AtomicBool,
    /// How long the host's replies take, as for a host some way off.
    round_trip_ms: AtomicU64,
}

/// The broker and the host behind it: one browser channel at a time, each socket one channel.
fn broker(
    listener: TcpListener,
    host: Arc<Mutex<FakeHost>>,
    log: Arc<Log>,
    origin: String,
    line: Arc<Line>,
    pending: Pending,
) {
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut ticket = String::new();
            // tungstenite's handshake callback returns its own `ErrorResponse`.
            #[allow(clippy::result_large_err)]
            let check = |request: &Request, mut response: Response| -> Result<Response, ErrorResponse> {
                let header = |name: &str| -> Vec<String> {
                    request.headers().get_all(name).iter().filter_map(|v| v.to_str().ok()).map(str::to_string).collect()
                };
                let offered: Vec<String> = header("Sec-WebSocket-Protocol")
                    .iter()
                    .flat_map(|v| v.split(','))
                    .map(|p| p.trim().to_string())
                    .collect();
                let refuse = || {
                    let mut refusal = ErrorResponse::new(None);
                    *refusal.status_mut() = tungstenite::http::StatusCode::FORBIDDEN;
                    refusal
                };
                if line.down.load(Ordering::SeqCst) {
                    return Err(refuse());
                }
                if header("Origin") != [origin.clone()] || !offered.iter().any(|p| p == "atomics.rendezvous.v1") {
                    return Err(refuse());
                }
                ticket = offered.iter().find_map(|p| p.strip_prefix("atomics.ticket.")).unwrap_or_default().to_string();
                response
                    .headers_mut()
                    .insert("Sec-WebSocket-Protocol", HeaderValue::from_static("atomics.rendezvous.v1"));
                Ok(response)
            };
            let Ok(mut socket) = tungstenite::accept_hdr(stream, check) else {
                log.push("upgrade refused");
                continue;
            };
            log.push(format!("dial {ticket}"));
            let _ = socket.get_mut().set_read_timeout(Some(Duration::from_millis(100)));
            host.lock().unwrap().join();
            let mut first = true;
            loop {
                if line.cut.swap(false, Ordering::SeqCst) {
                    let _ = socket.close(Some(CloseFrame { code: CloseCode::Away, reason: "".into() }));
                    let _ = socket.flush();
                    log.push("cut");
                    break;
                }
                for frame in std::mem::take(&mut *pending.lock().unwrap()) {
                    let _ = socket.send(tungstenite::Message::binary(frame));
                }
                let bytes = match socket.read() {
                    Ok(tungstenite::Message::Binary(bytes)) => bytes,
                    Ok(tungstenite::Message::Close(_)) => break,
                    Ok(_) => continue,
                    Err(tungstenite::Error::Io(e))
                        if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
                    {
                        continue;
                    }
                    Err(_) => break,
                };
                if first {
                    log.push(format!("first {} bytes, kind {}", bytes.len(), bytes[0]));
                    first = false;
                }
                let mut host = host.lock().unwrap();
                let before = host.received.len();
                let result = host.receive(&bytes);
                let closing = host.take_close();
                for message in &host.received[before..] {
                    match message {
                        Message::Hello { prior_epoch, .. } => {
                            log.push(format!("hello {}", prior_epoch.as_deref().unwrap_or("new")));
                        }
                        Message::Subscribe { topics } => {
                            for topic in topics {
                                if let Topic::Screen { session, .. } = topic {
                                    log.push(format!("subscribe {session}"));
                                }
                            }
                        }
                        Message::LeaseAcquire { session, takeover } => {
                            log.push(format!("acquire {session}{}", if *takeover { " takeover" } else { "" }));
                        }
                        Message::LeaseRelease { session, .. } => log.push(format!("release {session}")),
                        Message::TerminalResize { session, .. } => log.push(format!("resize {session}")),
                        _ => {}
                    }
                }
                drop(host);
                match result {
                    Ok(replies) => {
                        let round_trip = line.round_trip_ms.load(Ordering::SeqCst);
                        if round_trip > 0 && !replies.is_empty() {
                            thread::sleep(Duration::from_millis(round_trip));
                        }
                        for reply in replies {
                            let _ = socket.send(tungstenite::Message::binary(reply));
                        }
                        if let Some(code) = closing {
                            log.push(format!("host closed {code}"));
                            let _ = socket.close(Some(CloseFrame { code: CloseCode::from(code), reason: "".into() }));
                            let _ = socket.flush();
                            break;
                        }
                    }
                    Err(code) => {
                        log.push(format!("host closed {code}"));
                        let _ = socket.close(Some(CloseFrame { code: CloseCode::from(code), reason: "".into() }));
                        let _ = socket.flush();
                        break;
                    }
                }
            }
            host.lock().unwrap().left();
        }
    });
}

/// Unpadded or padded standard base64, as DevTools sends a screenshot.
fn base64(text: &str) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        _ => 63,
    };
    let (mut out, mut acc, mut bits) = (Vec::new(), 0u32, 0);
    for c in text.bytes().filter(|c| *c != b'=') {
        acc = (acc << 6) | u32::from(value(c));
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

/// Standard base64 with padding, to hand the page a PNG.
fn encode64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]) } else { '=' });
        }
    }
    out
}

/// A DevTools connection to one target of the browser (the page, or the client's frame), through the port
/// Chrome wrote into its profile. Every step is bounded by [`WAIT`], and every read by [`READ`].
struct DevTools {
    socket: tungstenite::WebSocket<TcpStream>,
    next: u64,
    /// Events read while waiting for something else, oldest first.
    events: VecDeque<serde_json::Value>,
}

impl DevTools {
    fn open(profile: &Path, what: &str, pick: impl Fn(&serde_json::Value) -> bool) -> DevTools {
        let deadline = Instant::now() + WAIT;
        let left = |step: &str| {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "devtools ({what}): timed out waiting for {step}");
            left
        };
        let port = loop {
            let active = std::fs::read_to_string(profile.join("DevToolsActivePort")).unwrap_or_default();
            if let Some(port) = active.lines().next().filter(|p| !p.trim().is_empty()) {
                break port.trim().to_string();
            }
            left("Chrome to write DevToolsActivePort");
            thread::sleep(Duration::from_millis(100));
        };
        let connect = |step: &str| {
            let stream = TcpStream::connect_timeout(&format!("127.0.0.1:{port}").parse().unwrap(), left(step))
                .unwrap_or_else(|e| panic!("devtools ({what}): {step}: {e}"));
            stream.set_read_timeout(Some(READ)).unwrap();
            stream.set_write_timeout(Some(READ)).unwrap();
            stream
        };
        let url = loop {
            // DevTools may keep the connection open: read the headers, then exactly Content-Length bytes.
            let mut http = BufReader::new(connect("the DevTools HTTP port"));
            write!(http.get_mut(), "GET /json/list HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").unwrap();
            let mut length = None;
            loop {
                let mut header = String::new();
                http.read_line(&mut header).unwrap_or_else(|e| panic!("devtools: reading /json/list's headers: {e}"));
                if header == "\r\n" || header.is_empty() {
                    break;
                }
                if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().ok();
                }
            }
            let mut body = vec![0; length.expect("devtools: /json/list has a Content-Length")];
            http.read_exact(&mut body).unwrap_or_else(|e| panic!("devtools: reading /json/list: {e}"));
            let targets: serde_json::Value = serde_json::from_slice(&body).expect("devtools: /json/list is JSON");
            let found = targets.as_array().unwrap().iter().find(|t| pick(t)).cloned();
            if let Some(url) = found.as_ref().and_then(|t| t["webSocketDebuggerUrl"].as_str()) {
                break url.to_string();
            }
            left("the target");
            thread::sleep(Duration::from_millis(200));
        };
        let (socket, _) = tungstenite::client(url.as_str(), connect("the DevTools socket"))
            .unwrap_or_else(|e| panic!("devtools ({what}): the handshake: {e}"));
        DevTools { socket, next: 0, events: VecDeque::new() }
    }

    /// The next message, whatever it is.
    fn read(&mut self, what: &str, deadline: Instant) -> serde_json::Value {
        loop {
            assert!(Instant::now() < deadline, "devtools: timed out waiting for {what}");
            match self.socket.read() {
                Ok(tungstenite::Message::Text(text)) => return serde_json::from_str(text.as_str()).unwrap(),
                Ok(_) => continue,
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
                {
                    continue;
                }
                Err(e) => panic!("devtools: the socket: {e}"),
            }
        }
    }

    /// Call `method` and return its result.
    fn call(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.call_in(None, method, params)
    }

    /// Call `method` in a target this connection attached to (`session`), or in its own (`None`).
    fn call_in(&mut self, session: Option<&str>, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.next += 1;
        let id = self.next;
        let mut ask = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            ask["sessionId"] = json!(session);
        }
        self.socket.send(tungstenite::Message::text(ask.to_string())).unwrap();
        let deadline = Instant::now() + WAIT;
        loop {
            let reply = self.read(method, deadline);
            if reply["id"] == id {
                assert!(reply.get("error").is_none(), "devtools: {method} failed: {reply}");
                return reply["result"].clone();
            }
            if reply.get("method").is_some() {
                self.events.push_back(reply);
            }
        }
    }

    /// Wait for an event `method` that `pick` takes.
    fn event(&mut self, method: &str, pick: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let wanted = |e: &serde_json::Value| e["method"] == method && pick(e);
        if let Some(at) = self.events.iter().position(wanted) {
            return self.events.remove(at).expect("found");
        }
        let deadline = Instant::now() + WAIT;
        loop {
            let event = self.read(method, deadline);
            if wanted(&event) {
                return event;
            }
            if event.get("method").is_some() {
                self.events.push_back(event);
            }
        }
    }

    /// The value of a JavaScript expression in the target.
    fn eval(&mut self, expression: &str) -> serde_json::Value {
        let params = json!({ "expression": expression, "returnByValue": true, "awaitPromise": true });
        let result = self.call("Runtime.evaluate", params);
        result["result"]["value"].clone()
    }

    /// Wait until `expression` is true in the target, naming `what` if it never is.
    fn wait(&mut self, what: &str, expression: &str) {
        let deadline = Instant::now() + WAIT;
        while self.eval(expression) != json!(true) {
            assert!(Instant::now() < deadline, "timed out waiting for {what} ({expression})");
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn mouse(&mut self, kind: &str, (x, y): (f64, f64)) {
        let pressed = kind != "mouseMoved";
        let params = json!({
            "type": kind, "x": x, "y": y, "button": if pressed { "left" } else { "none" },
            "buttons": if kind == "mouseReleased" { 0 } else { 1 }, "clickCount": if pressed { 1 } else { 0 },
        });
        self.call("Input.dispatchMouseEvent", params);
    }

    fn click(&mut self, at: (f64, f64)) {
        self.mouse("mousePressed", at);
        self.mouse("mouseReleased", at);
    }

    /// A click of the right button, which opens a row's menu.
    fn right_click(&mut self, (x, y): (f64, f64)) {
        for (kind, buttons) in [("mousePressed", 2), ("mouseReleased", 0)] {
            let event = json!({ "type": kind, "x": x, "y": y, "button": "right", "buttons": buttons, "clickCount": 1 });
            self.call("Input.dispatchMouseEvent", event);
        }
    }

    /// A press at `from`, a move to `to`, and a release.
    fn mouse_drag(&mut self, from: (f64, f64), to: (f64, f64)) {
        self.mouse("mouseMoved", from);
        self.mouse("mousePressed", from);
        thread::sleep(Duration::from_millis(100));
        for step in 1..=5 {
            let t = f64::from(step) / 5.0;
            self.mouse("mouseMoved", (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t));
            thread::sleep(Duration::from_millis(60));
        }
        self.mouse("mouseReleased", to);
    }

    /// A finger touches, moves or lifts: `points` are the fingers still on the screen.
    fn touch(&mut self, kind: &str, points: &[Pos2]) {
        let points: Vec<serde_json::Value> = points
            .iter()
            .map(|p| json!({ "x": p.x, "y": p.y, "radiusX": 4, "radiusY": 4, "force": 1, "id": 1 }))
            .collect();
        self.call("Input.dispatchTouchEvent", json!({ "type": kind, "touchPoints": points }));
    }

    /// A finger taps `at`, as briefly as a person's does.
    fn tap(&mut self, at: Pos2) {
        self.touch("touchStart", &[at]);
        thread::sleep(Duration::from_millis(80));
        self.touch("touchEnd", &[]);
    }

    /// A finger rests on `at` long enough for a long press, and lifts.
    fn long_press(&mut self, at: Pos2) {
        self.touch("touchStart", &[at]);
        thread::sleep(Duration::from_millis(900));
        self.touch("touchEnd", &[]);
    }

    /// A finger goes down at `from`, moves to `to` in a few steps, and lifts.
    fn drag(&mut self, from: Pos2, to: Pos2) {
        self.touch("touchStart", &[from]);
        for step in 1..=6 {
            thread::sleep(Duration::from_millis(40));
            self.touch("touchMove", &[from + (to - from) * (step as f32 / 6.0)]);
        }
        thread::sleep(Duration::from_millis(40));
        self.touch("touchEnd", &[]);
    }

    /// A key pressed and let go on the focused field, as a soft keyboard sends Return or Backspace.
    fn key(&mut self, key: &str, code: u32, text: Option<&str>) {
        let mut down = json!({ "type": "keyDown", "key": key, "code": key, "windowsVirtualKeyCode": code });
        if let Some(text) = text {
            down["text"] = json!(text);
        }
        self.call("Input.dispatchKeyEvent", down);
        self.call(
            "Input.dispatchKeyEvent",
            json!({ "type": "keyUp", "key": key, "code": key, "windowsVirtualKeyCode": code }),
        );
    }
}

/// Save what the page shows.
fn screenshot(profile: &Path, out: &Path) {
    let mut page = DevTools::open(profile, "screenshot", |t| t["type"] == "page");
    let data = page.call("Page.captureScreenshot", json!({ "format": "png" }));
    let data = data["data"].as_str().unwrap_or_else(|| panic!("screenshot: DevTools said {data}"));
    std::fs::write(out, base64(data)).unwrap();
}

/// Wait until `ready` holds for the host, naming `what` if it never does.
fn wait_host(host: &Mutex<FakeHost>, what: &str, ready: impl Fn(&FakeHost) -> bool) {
    let deadline = Instant::now() + WAIT;
    while !ready(&host.lock().unwrap()) {
        assert!(Instant::now() < deadline, "timed out waiting for the host: {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

fn chrome() -> String {
    if let Ok(chrome) = std::env::var("CHROME") {
        return chrome;
    }
    ["google-chrome-stable", "google-chrome", "chromium", "chromium-browser"]
        .into_iter()
        .find(|name| Command::new(name).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok())
        .expect("Chrome or Chromium on PATH, or CHROME")
        .to_string()
}

/// The bundle to serve: `ATOMICS_WEB_DIST`, else the crate's `dist`.
fn dist() -> PathBuf {
    let dist = std::env::var_os("ATOMICS_WEB_DIST")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("dist"));
    assert!(dist.join("index.html").is_file(), "no bundle in {}: run scripts/build-web.sh", dist.display());
    dist
}

/// The client's origin: the bundle under the strict CSP it is built for, beside the hosting page's config,
/// which names `parent_origin`; the broker listens on `tunnel_port`.
fn serve_client(listener: TcpListener, dist: PathBuf, parent_origin: &str, tunnel_port: u16) {
    let csp = format!(
        "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; connect-src 'self' ws://127.0.0.1:{tunnel_port}; frame-ancestors {parent_origin}"
    );
    let config = format!("window.ATOMICS_PARENT_ORIGIN = '{parent_origin}';\n");
    serve(
        listener,
        Arc::new(move |_, path, _| {
            let path = if path == "/" { "/index.html" } else { path };
            if path == "/config.js" {
                return Some(Reply::new("text/javascript", config.clone()));
            }
            let file = path.trim_start_matches('/');
            if file.split('/').any(|part| part == ".." || part.is_empty()) {
                return None;
            }
            let mut reply = Reply::new(content_type(file), std::fs::read(dist.join(file)).ok()?);
            if file == "index.html" {
                reply.headers.push(("Content-Security-Policy", csp.clone()));
            }
            Some(reply)
        }),
    );
}

/// The hosting page's origin: `pages` by path, `/phase` (what the page is to do next) and `/report` (what its
/// frame said, logged as `parent …`).
fn serve_parent(listener: TcpListener, pages: HashMap<&'static str, String>, log: Arc<Log>, phase: Arc<Mutex<String>>) {
    serve(
        listener,
        Arc::new(move |method, path, body| match (method, path) {
            ("GET", "/phase") => Some(Reply::new("text/plain", phase.lock().unwrap().clone())),
            ("GET", page) => pages.get(page).map(|html| Reply::new("text/html; charset=utf-8", html.clone())),
            ("POST", "/report") => {
                log.push(format!("parent {}", String::from_utf8_lossy(body)));
                Some(Reply::new("text/plain", ""))
            }
            _ => None,
        }),
    );
}

/// Headless Chrome on `url` in a fresh profile, with `flags`; the watchdog stops it after `overall`.
fn launch(flags: &[String], url: &str, log: &Arc<Log>, overall: Duration) -> Browser {
    let profile = tempfile::tempdir().unwrap();
    let chrome_log = profile.path().join("chrome.log");
    let child = Command::new(chrome())
        .args([
            "--headless=new".to_string(),
            format!("--user-data-dir={}", profile.path().join("profile").display()),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            "--disable-extensions".into(),
            "--use-angle=swiftshader".into(),
            "--enable-unsafe-swiftshader".into(),
            "--enable-logging=stderr".into(),
            "--remote-debugging-port=0".into(),
        ])
        .args(flags)
        .arg(url)
        .stdout(Stdio::null())
        .stderr(File::create(&chrome_log).unwrap())
        .spawn()
        .expect("Chrome starts");
    let child = Arc::new(Mutex::new(child));
    watchdog(child.clone(), log.clone(), chrome_log.clone(), overall);
    Browser { child, log: chrome_log, profile }
}

#[test]
#[ignore = "needs the built bundle (scripts/build-web.sh) and Chrome"]
fn the_bundle_pairs_reconnects_and_reloads_in_a_browser() {
    let dist = dist();
    let mut host = fake_host::host();
    let pairing = host.open_pairing();
    let host = Arc::new(Mutex::new(host));
    let log = Arc::new(Log::default());
    let bind = || TcpListener::bind("127.0.0.1:0").unwrap();
    let (client, parent, tunnel) = (bind(), bind(), bind());
    let port = |l: &TcpListener| l.local_addr().unwrap().port();
    let client_origin = format!("http://127.0.0.1:{}", port(&client));
    // The parent's server answers as `localhost` and, for the attacker's frame, as `127.0.0.1`: two origins.
    let parent_origin = format!("http://localhost:{}", port(&parent));
    let other_origin = format!("http://127.0.0.1:{}", port(&parent));
    let broker_url = format!("ws://127.0.0.1:{}/atomics/tunnel", port(&tunnel));

    let fragment = pairing.fragment();
    let keys: Vec<&str> = fragment.split('.').collect();
    let boot = |ticket: &str| {
        json!({
            "type": "atomics.embed.boot", "v": 1, "host_id": HOST_ID, "ticket": ticket, "broker": broker_url,
            "display_name": "A Person", "theme": "dark", "scheme": "dark", "locale": "en", "view": "board",
        })
    };
    let mut pair = boot("ticket-0");
    pair["mode"] = json!("pair");
    pair["pair"] = json!({ "v": 1, "host_noise_pubkey": keys[2], "psk": keys[3] });
    // The reloaded page opens on an agent's terminal.
    let mut session = boot("ticket-s");
    session["view"] = json!("terminal");
    let evil = boot("evil");

    serve_client(client, dist, &parent_origin, port(&tunnel));
    let phase = Arc::new(Mutex::new("pair".to_string()));
    let page = include_str!("browser/parent.html")
        .replace("CLIENT_ORIGIN", &client_origin)
        .replace("OTHER_ORIGIN", &other_origin)
        .replace("PAIR_BOOT", &pair.to_string())
        .replace("SESSION_BOOT", &session.to_string());
    let attacker = include_str!("browser/other.html").replace("EVIL_BOOT", &evil.to_string());
    serve_parent(parent, HashMap::from([("/", page), ("/other", attacker)]), log.clone(), phase.clone());
    let line = Arc::new(Line::default());
    let pending: Pending = Arc::default();
    broker(tunnel, host.clone(), log.clone(), client_origin.clone(), line.clone(), pending.clone());

    let browser = launch(&["--window-size=420,900".into()], &format!("{parent_origin}/"), &log, OVERALL);
    let devtools = browser.devtools();

    // Pairing: the boot, the IKpsk2 channel, the host's answer, then hello on a board this page never had.
    log.wait("parent atomics.ready", 1);
    log.wait("parent atomics.booted", 1);
    log.wait("dial ticket-0", 1);
    log.wait("first 97 bytes, kind 1", 1);
    log.wait("parent atomics.paired", 1);
    log.wait("hello new", 1);
    assert_eq!(host.lock().unwrap().allowed.len(), 1, "the host holds the browser's key");

    // The broker cuts the channel: a fresh ticket from the parent, the stored key, and a resume.
    line.cut.store(true, Ordering::SeqCst);
    log.wait("cut", 1);
    log.wait("parent atomics.ticket", 1);
    log.wait("dial ticket-1", 1);
    log.wait("first 97 bytes, kind 2", 1);
    log.wait("hello epoch-1", 1);
    if let Some(out) = std::env::var_os("ATOMICS_WEB_SCREENSHOT") {
        thread::sleep(Duration::from_secs(2));
        screenshot(&devtools, Path::new(&out));
    }

    // A new page: the key comes back out of IndexedDB, and the host still knows it.
    *phase.lock().unwrap() = "session".into();
    log.wait("parent atomics.ready", 2);
    log.wait("parent atomics.booted", 2);
    log.wait("dial ticket-s", 1);
    log.wait("first 97 bytes, kind 2", 2);
    log.wait("hello new", 2);

    // The terminal: following it takes no lease, and nothing ever resizes it.
    log.wait("subscribe s1", 1);
    assert!(!log.has("acquire s1"), "following a terminal never takes its lease");
    let frame_url = format!("{client_origin}/");
    let mut frame = DevTools::open(&devtools, "the client's frame", |t| {
        t["type"] == "iframe" && t["url"].as_str().is_some_and(|u| u.starts_with(&frame_url))
    });
    let mut page = DevTools::open(&devtools, "the hosting page", |t| t["type"] == "page");
    frame.wait("the input overlay over the terminal", "!document.getElementById('atomics-input').hidden");
    let offset = page.eval(
        "(() => { const f = document.querySelector('iframe'); const r = f.getBoundingClientRect(); \
         return [r.left + f.clientLeft, r.top + f.clientTop]; })()",
    );
    let overlay = frame.eval("(() => { const r = document.getElementById('atomics-input').getBoundingClientRect(); return [r.left, r.top, r.width]; })()");
    let number = |v: &serde_json::Value, i: usize| v[i].as_f64().expect("a number");
    let at =
        |x: f64, y: f64| (number(&offset, 0) + number(&overlay, 0) + x, number(&offset, 1) + number(&overlay, 1) + y);
    assert!(number(&overlay, 2) >= 380.0, "the terminal fills the phone frame's width: {overlay}");

    // A click focuses the overlay, which asks for the lease; typing and Return reach the host.
    frame.wait("a writable overlay", "!document.getElementById('atomics-input').readOnly");
    page.click(at(40.0, 40.0));
    log.wait("acquire s1", 1);
    frame.wait("the focus on the overlay", "document.activeElement === document.getElementById('atomics-input')");
    page.call("Input.insertText", json!({ "text": "echo hi" }));
    let enter = |kind: &str| json!({ "type": kind, "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "text": "\r" });
    page.call("Input.dispatchKeyEvent", enter("keyDown"));
    page.call(
        "Input.dispatchKeyEvent",
        json!({ "type": "keyUp", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13 }),
    );
    wait_host(&host, "\"echo hi\\r\" typed", |h| h.terminals["s1"].typed == "echo hi\r");

    // The echo is on the client's screen: a drag over the first line selects it from the client's cells.
    page.mouse("mousePressed", at(5.0, 6.0));
    page.mouse("mouseMoved", at(200.0, 6.0));
    page.mouse("mouseMoved", at(360.0, 6.0));
    page.mouse("mouseReleased", at(360.0, 6.0));
    frame.wait("the echoed line selected", "document.getElementById('atomics-copy').title.includes('$ echo hi')");
    if let Some(out) = std::env::var_os("ATOMICS_WEB_TERMINAL_SCREENSHOT") {
        thread::sleep(Duration::from_millis(500));
        screenshot(&devtools, Path::new(&out));
    }

    // The desktop types: the lease goes back and the overlay loses the focus; a new click takes it again.
    let back = host.lock().unwrap().desktop_types("s1", "!");
    pending.lock().unwrap().extend(back);
    frame.wait("the overlay blurred", "document.activeElement !== document.getElementById('atomics-input')");
    assert!(host.lock().unwrap().terminals["s1"].lease.is_none());
    page.click(at(40.0, 40.0));
    log.wait("acquire s1", 2);
    frame.wait("the focus back on the overlay", "document.activeElement === document.getElementById('atomics-input')");
    page.call("Input.insertText", json!({ "text": "x" }));
    wait_host(&host, "the second typing", |h| h.terminals["s1"].typed == "echo hi\r!x");
    assert!(!log.has("resize s1"), "the phone never resized the terminal");

    log.wait("parent other posted", 1);
    assert!(!log.has("dial evil"), "a boot from another origin was never taken");
    assert!(!log.has("upgrade refused"), "every upgrade carried the client's exact origin and the subprotocol");
    assert!(!log.lines.lock().unwrap().iter().any(|l| l.starts_with("host closed")), "no channel was refused");
}

/// Screenshot the phone and keep it in the page for [`boxes`]: until `find` finds something on one, which is
/// returned with that screenshot (a base64 PNG).
fn until<T>(page: &mut DevTools, what: &str, find: impl Fn(&mut DevTools) -> Option<T>) -> (T, String) {
    until_on(page, (PHONE.0 * PHONE_SCALE, PHONE.1 * PHONE_SCALE), &shots(), what, find)
}

/// [`until`] on a screen of `pixels`, saving the last screenshot in `dir` if it times out.
fn until_on<T>(
    page: &mut DevTools,
    pixels: (f32, f32),
    dir: &Path,
    what: &str,
    find: impl Fn(&mut DevTools) -> Option<T>,
) -> (T, String) {
    let deadline = Instant::now() + WAIT;
    loop {
        let shot = page.call("Page.captureScreenshot", json!({ "format": "png" }));
        let png = shot["data"].as_str().unwrap_or_else(|| panic!("screenshot: DevTools said {shot}")).to_string();
        let size = page.eval(&format!("look('{png}')"));
        let size = (size[0].as_f64(), size[1].as_f64());
        let expected = (f64::from(pixels.0), f64::from(pixels.1));
        assert_eq!(size, (Some(expected.0), Some(expected.1)), "a screenshot of the screen's pixels");
        if let Some(found) = find(page) {
            return (found, png);
        }
        if Instant::now() >= deadline {
            let out = dir.join("timed-out.png");
            std::fs::write(&out, base64(&png)).unwrap();
            panic!("timed out waiting for {what} on the screen; it showed {}", out.display());
        }
        thread::sleep(Duration::from_millis(150));
    }
}

/// The areas of `color` (±`tol` a channel) inside `region` on the last screenshot, in points, largest first.
fn boxes(page: &mut DevTools, color: Color32, tol: u8, region: Rect, min_area: f32) -> Vec<Rect> {
    let [r, g, b, _] = color.to_srgba_unmultiplied();
    let (min, max) = (region.min, region.max);
    let found =
        page.eval(&format!("boxes([{r}, {g}, {b}], {tol}, [{}, {}, {}, {}], {min_area})", min.x, min.y, max.x, max.y));
    let found = found.as_array().unwrap_or_else(|| panic!("boxes: the page said {found}"));
    found
        .iter()
        .map(|b| {
            let n = |i: usize| b[i].as_f64().expect("a number") as f32;
            Rect::from_min_size(pos2(n(0), n(1)), vec2(n(2), n(3)))
        })
        .collect()
}

/// How much of `region` shows `color` on the last screenshot, in square points: text in that colour, say.
fn amount(page: &mut DevTools, color: Color32, tol: u8, region: Rect) -> f64 {
    let [r, g, b, _] = color.to_srgba_unmultiplied();
    let (min, max) = (region.min, region.max);
    let found = page.eval(&format!("boxes([{r}, {g}, {b}], {tol}, [{}, {}, {}, {}], 1)", min.x, min.y, max.x, max.y));
    found
        .as_array()
        .unwrap_or_else(|| panic!("boxes: the page said {found}"))
        .iter()
        .filter_map(|b| b[4].as_f64())
        .sum()
}

/// The phone's screens, each saved as `NN-name.png` in `target/phone-shots` for a person to look at.
struct Screens {
    dir: PathBuf,
    saved: Vec<PathBuf>,
}

/// `target/phone-shots`.
fn shots() -> PathBuf {
    target().join("phone-shots")
}

/// The target folder: the test runs as `target/<profile>/deps/browser-<hash>`.
fn target() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.ancestors().nth(3).expect("the target folder").to_path_buf()
}

impl Screens {
    fn new() -> Self {
        let dir = shots();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir, saved: Vec::new() }
    }

    /// Wait for the screen `ready` sees, and save it.
    fn save(&mut self, page: &mut DevTools, name: &str, ready: impl Fn(&mut DevTools) -> bool) {
        let ((), png) = until(page, &format!("the {name} screen"), |page| ready(page).then_some(()));
        self.keep(&png, name);
    }

    /// Save a screenshot already taken (a base64 PNG).
    fn keep(&mut self, png: &str, name: &str) {
        let path = self.dir.join(format!("{:02}-{name}.png", self.saved.len() + 1));
        std::fs::write(&path, base64(png)).unwrap();
        self.saved.push(path);
    }
}

/// What the browser typed into `session`'s terminal: the bytes of every `terminal_input` the host took.
fn typed(host: &Mutex<FakeHost>, session: &str) -> String {
    host.lock().unwrap().terminals[session].typed.clone()
}

/// How many `terminal_input` messages the host received.
fn inputs(host: &Mutex<FakeHost>) -> usize {
    host.lock().unwrap().received.iter().filter(|m| matches!(m, Message::TerminalInput { .. })).count()
}

#[test]
#[ignore = "needs the built bundle (scripts/build-web.sh) and Chrome"]
fn the_bundle_works_on_a_phone_by_touch() {
    let dist = dist();
    let mut host = fake_host::host();
    // The pairing code stays on screen until the test has looked at it.
    host.hold = true;
    let pairing = host.open_pairing();
    let host = Arc::new(Mutex::new(host));
    let log = Arc::new(Log::default());
    let bind = || TcpListener::bind("127.0.0.1:0").unwrap();
    let (client, parent, tunnel) = (bind(), bind(), bind());
    let port = |l: &TcpListener| l.local_addr().unwrap().port();
    let client_origin = format!("http://127.0.0.1:{}", port(&client));
    let parent_origin = format!("http://localhost:{}", port(&parent));
    let broker_url = format!("ws://127.0.0.1:{}/atomics/tunnel", port(&tunnel));
    let fragment = pairing.fragment();
    let keys: Vec<&str> = fragment.split('.').collect();
    let pair = json!({
        "type": "atomics.embed.boot", "v": 1, "host_id": HOST_ID, "ticket": "ticket-0", "broker": broker_url,
        "display_name": "A Person", "theme": "dark", "scheme": "dark", "locale": "en", "view": "board",
        "mode": "pair", "pair": { "v": 1, "host_noise_pubkey": keys[2], "psk": keys[3] },
    });

    serve_client(client, dist, &parent_origin, port(&tunnel));
    let phase = Arc::new(Mutex::new("wait".to_string()));
    let page_html = include_str!("browser/phone.html")
        .replace("CLIENT_ORIGIN", &client_origin)
        .replace("PHONE_BOOT", &pair.to_string());
    serve_parent(parent, HashMap::from([("/", page_html)]), log.clone(), phase.clone());
    let pending: Pending = Arc::default();
    broker(tunnel, host.clone(), log.clone(), client_origin.clone(), Arc::default(), pending.clone());

    // The phone. The client's frame is cross-origin, so it draws in a renderer of its own, which the page's
    // emulation does not reach: its pixel ratio comes from the browser's flag, its user agent too, and its touch
    // screen from its own target, attached and set up before its first script runs.
    let flags = [
        format!("--window-size={},{}", PHONE.0, PHONE.1),
        format!("--force-device-scale-factor={PHONE_SCALE}"),
        format!("--user-agent={IOS_SAFARI}"),
        "--hide-scrollbars".into(),
    ];
    let browser = launch(&flags, &format!("{parent_origin}/"), &log, PHONE_OVERALL);
    let devtools = browser.devtools();
    let mut page = DevTools::open(&devtools, "the phone's page", |t| t["type"] == "page");
    let (width, height) = (PHONE.0 as u32, PHONE.1 as u32);
    let metrics = json!({ "width": width, "height": height, "deviceScaleFactor": PHONE_SCALE, "mobile": true });
    page.call("Emulation.setDeviceMetricsOverride", metrics);
    let touch = json!({ "enabled": true, "maxTouchPoints": 5 });
    page.call("Emulation.setTouchEmulationEnabled", touch.clone());
    page.call("Target.setAutoAttach", json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }));
    *phase.lock().unwrap() = "go".into();
    let attached = page.event("Target.attachedToTarget", |e| e["params"]["targetInfo"]["type"] == "iframe");
    let session = attached["params"]["sessionId"].as_str().expect("the frame's session").to_string();
    page.call_in(Some(&session), "Emulation.setTouchEmulationEnabled", touch);
    page.call_in(Some(&session), "Runtime.runIfWaitingForDebugger", json!({}));
    let frame_url = format!("{client_origin}/");
    let mut frame = DevTools::open(&devtools, "the client's frame", |t| {
        t["type"] == "iframe" && t["url"].as_str().is_some_and(|u| u.starts_with(&frame_url))
    });
    let device = "devicePixelRatio === 3 && matchMedia('(pointer: coarse)').matches && navigator.maxTouchPoints > 0";
    frame.wait("an iPhone's screen in the frame", device);
    let offset = page.eval("(() => { const r = document.querySelector('iframe').getBoundingClientRect(); return [r.left, r.top, r.width, r.height]; })()");
    let frame_rect: Vec<f64> = (0..4).filter_map(|i| offset[i].as_f64()).collect();
    assert_eq!(frame_rect, [0.0, 0.0, f64::from(width), f64::from(height)], "the frame fills the screen");

    // What the client draws with: the boot's theme, and the phone layout of its own code, in its zoomed points
    // and then in CSS pixels, which the screenshots and taps are in.
    let parent = ParentOrigin::parse(&parent_origin).expect("a loopback origin");
    let Ok(FromParent::Boot(booted)) = boot::read(&parent, &parent_origin, true, &pair) else { panic!("the boot") };
    let p = Palette::new(&booted.theme_tokens(), Fonts::default());
    assert_ne!(p.panel, p.background, "the approval banner stands out from the page");
    let css = |r: Rect| {
        Rect::from_min_max((r.min.to_vec2() * layout::ZOOM).to_pos2(), (r.max.to_vec2() * layout::ZOOM).to_pos2())
    };
    let points = Rect::from_min_size(Pos2::ZERO, vec2(PHONE.0, PHONE.1) / layout::ZOOM);
    let regions =
        layout::regions(points, &Viewport { visible: Some(points), coarse: true, ..Viewport::default() }, false);
    let bar = regions.bar.expect("the pane bar");
    let items: Vec<Rect> = layout::bar_items(bar, views::BAR.len()).into_iter().map(css).collect();
    let (screen, top, body, bar) = (css(points), css(regions.top), css(regions.body), css(bar));
    let pane = |tab: Tab| items[views::BAR.iter().position(|(t, _)| *t == tab).expect("a pane")].center();
    // The pane the bar marks, by its accent line on the bar's top edge; none while the key bar replaces it.
    let active = |page: &mut DevTools| {
        let edge = Rect::from_min_max(bar.left_top(), pos2(bar.right(), bar.top() + 2.0));
        let line = boxes(page, p.accent, 8, edge, 10.0).first().map(|b| b.center().x);
        line.and_then(|x| items.iter().position(|i| i.x_range().contains(x))).map(|i| views::BAR[i].0)
    };
    // The plan's Approve button: the accent-filled box, a finger's size, in the board's header rows.
    let header = Rect::from_min_max(body.left_top(), pos2(screen.right(), body.top() + 220.0));
    let approve = |page: &mut DevTools| {
        boxes(page, p.accent, 6, header, 150.0).into_iter().find(|b| b.width() >= 40.0 && b.height() >= 40.0)
    };
    // A tool request's banner, and its first button (Allow).
    let banner = |page: &mut DevTools| {
        boxes(page, p.panel, 2, body, 5000.0).into_iter().find(|b| b.width() >= 300.0 && b.height() >= 100.0)
    };
    let mut screens = Screens::new();

    // 1. Pairing: the six-digit code, large in the accent colour, until the host's user accepts it.
    log.wait("parent atomics.ready", 1);
    log.wait("parent atomics.booted", 1);
    log.wait("dial ticket-0", 1);
    log.wait("first 97 bytes, kind 1", 1);
    wait_host(&host, "the pairing request", |h| h.named.is_some());
    assert_eq!(host.lock().unwrap().named.as_deref(), Some("Safari on iPhone"), "the frame has Safari's user agent");
    screens.save(&mut page, "pairing", |page| amount(page, p.accent, 24, body) >= 300.0);
    let answer = host.lock().unwrap().confirm();
    pending.lock().unwrap().extend(answer);
    log.wait("parent atomics.paired", 1);
    log.wait("hello new", 1);

    // 2. The board with its plan, the plans, and the task's diff of two files, each pane tapped in the bar.
    screens.save(&mut page, "board", |page| active(page) == Some(Tab::Board) && approve(page).is_some());
    page.tap(pane(Tab::Agents));
    screens.save(&mut page, "agents", |page| active(page) == Some(Tab::Agents));
    page.tap(pane(Tab::Diffs));
    screens.save(&mut page, "diffs", |page| active(page) == Some(Tab::Diffs));
    // The worktree's row, first in the list, opens its diff.
    let first =
        css(Rect::from_min_size(regions.body.left_top() + vec2(layout::GUTTER, 8.0), vec2(80.0, layout::TOUCH)));
    page.tap(first.center());
    wait_host(&host, "both files of the diff asked for", |h| {
        let asked = |file: &str| h.received.iter().any(|m| matches!(m, Message::DiffGet { path, .. } if path == file));
        asked("src/a.rs") && asked("src/b.rs")
    });
    screens.save(&mut page, "diff", |page| !boxes(page, p.added, 2, body, 2000.0).is_empty());

    // 3. Approving: a tap on the plan's Approve button sends the plan's approval with its digest, and the board
    // shows it approved. Then the top bar's ◆ badge opens the tool request, and a tap on Allow answers it.
    page.tap(pane(Tab::Board));
    let (button, _) = until(&mut page, "the plan's Approve button", |page| approve(page));
    page.tap(button.center());
    wait_host(&host, "plan_command approve for p1 at its digest", |h| {
        h.received.iter().any(|m| {
            matches!(m, Message::PlanCommand(c) if c.action == PlanAction::Approve
                && c.plan.as_deref() == Some("p1") && c.digest.as_deref() == Some("digest-1"))
        })
    });
    wait_host(&host, "the plan approved", |h| h.board.plans[0].approved);
    screens.save(&mut page, "plan-approved", |page| {
        // "approved ✓", wherever the header's rows put it.
        approve(page).is_none() && amount(page, p.success, 24, header) >= 10.0
    });
    let corner = Rect::from_min_max(pos2(screen.center().x, top.top()), top.max);
    let (badge, _) = until(&mut page, "the approvals badge", |page| {
        boxes(page, p.warning, 16, corner, 1.0).into_iter().reduce(|a, b| a.union(b))
    });
    page.tap(badge.center());
    screens.save(&mut page, "approvals", |page| banner(page).is_some());
    let (allow, _) = until(&mut page, "the Allow button", |page| {
        let banner = banner(page)?;
        let buttons = boxes(page, p.raised, 2, banner, 100.0).into_iter();
        // A finger's target: 44 CSS pixels each way, less the outline.
        buttons
            .filter(|b| b.width() >= 40.0 && (40.0..=50.0).contains(&b.height()))
            .min_by(|a, b| (a.top() as i32, a.left() as i32).cmp(&(b.top() as i32, b.left() as i32)))
    });
    page.tap(allow.center());
    wait_host(&host, "approval_decide allow for a1", |h| {
        h.received
            .iter()
            .any(|m| matches!(m, Message::ApprovalDecide(d) if d.approval == "a1" && d.decision == Decision::Allow))
    });
    screens.save(&mut page, "approval-allowed", |page| banner(page).is_none());

    // 4. The agent's terminal: following it takes no lease; a tap on it focuses the input overlay, which asks
    // for the lease, and the key bar takes the pane bar's place.
    page.tap(pane(Tab::Terminal));
    log.wait("subscribe s1", 1);
    frame.wait("the input overlay over the terminal", "!document.getElementById('atomics-input').hidden");
    frame.wait("a writable overlay", "!document.getElementById('atomics-input').readOnly");
    assert!(!log.has("acquire s1"), "following a terminal never takes its lease");
    screens.save(&mut page, "terminal", |page| active(page) == Some(Tab::Terminal));
    let overlay = frame.eval("(() => { const r = document.getElementById('atomics-input').getBoundingClientRect(); return [r.left, r.top, r.width, r.height]; })()");
    let number = |v: &serde_json::Value, i: usize| v[i].as_f64().expect("a number") as f32;
    assert!(number(&overlay, 2) >= PHONE.0 - 1.0, "the terminal fills the phone's width: {overlay}");
    page.tap(pos2(number(&overlay, 0) + 40.0, number(&overlay, 1) + 40.0));
    let focused = "document.activeElement === document.getElementById('atomics-input')";
    frame.wait("the focus on the overlay after a tap", focused);
    log.wait("acquire s1", 1);
    frame.wait("the key bar", "!document.getElementById('atomics-keybar').hidden");
    screens.save(&mut page, "terminal-focused", |page| active(page).is_none());

    // 5. What the phone types reaches the host as its bytes: text, Return, Backspace on the empty line the
    // echo left (the field is empty too), and the key bar's ↑, a finger's size, which leaves the focus where
    // it was.
    let words = "echo hello world";
    page.call("Input.insertText", json!({ "text": words }));
    wait_host(&host, "the words typed", |h| h.terminals["s1"].typed == words);
    page.key("Enter", 13, Some("\r"));
    wait_host(&host, "Return as CR", |h| h.terminals["s1"].typed == format!("{words}\r"));
    page.key("Backspace", 8, None);
    wait_host(&host, "Backspace as DEL", |h| h.terminals["s1"].typed == format!("{words}\r\x7f"));
    let up = frame.eval("(() => { const r = document.querySelector('#atomics-keybar button[data-value=\"ArrowUp\"]').getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2, r.width, r.height]; })()");
    assert!(number(&up, 2) >= 44.0 && number(&up, 3) >= 44.0, "a key bar key is a finger's target: {up}");
    page.tap(pos2(number(&up, 0), number(&up, 1)));
    wait_host(&host, "↑ as CSI A", |h| h.terminals["s1"].typed == format!("{words}\r\x7f\x1b[A"));
    assert_eq!(frame.eval(focused), json!(true), "a key bar key leaves the focus on the field");
    assert_eq!(inputs(&host), 4, "one terminal_input each");
    screens.save(&mut page, "typed", |page| active(page).is_none());

    // 6. The desktop takes the terminal back: the field loses the focus, the key bar goes, and nothing the
    // phone types reaches the host. Text a keyboard commits (`insertText`) does not put the focus back, since
    // the caret went with it, nor do Return and Backspace.
    let back = host.lock().unwrap().desktop_types("s1", "!");
    pending.lock().unwrap().extend(back);
    frame.wait("the field blurred", &format!("!({focused})"));
    frame.wait("the key bar gone", "document.getElementById('atomics-keybar').hidden");
    page.call("Input.insertText", json!({ "text": "x" }));
    page.key("Enter", 13, Some("\r"));
    page.key("Backspace", 8, None);
    screens.save(&mut page, "taken-back", |page| active(page) == Some(Tab::Terminal));
    thread::sleep(Duration::from_millis(1000));
    assert_eq!(frame.eval(focused), json!(false), "typing did not focus the field again");
    let all = format!("{words}\r\x7f\x1b[A!");
    assert_eq!(typed(&host, "s1"), all, "after the take back nothing typed reaches the host");
    assert_eq!(inputs(&host), 4, "no terminal_input after the take back");
    assert!(host.lock().unwrap().terminals["s1"].lease.is_none(), "the desktop holds the terminal");

    // 7. Selecting by touch: a long press on a word selects that word alone, with a handle at each end; the end
    // handle dragged over the next word takes it in, and Copy copies exactly that. The first line reads
    // "$ echo hello world", in the terminal's foreground; its ink gives the cells' width.
    let cells = Rect::from_min_size(
        pos2(number(&overlay, 0), number(&overlay, 1)),
        vec2(number(&overlay, 2), number(&overlay, 3)),
    );
    let ink = Color32::from_rgb(220, 216, 208);
    let first_line = Rect::from_min_max(cells.min, pos2(cells.right(), cells.top() + 60.0));
    let (text, _) = until(&mut page, "the first line's text", |page| {
        let glyphs = boxes(page, ink, 40, first_line, 2.0);
        let top = glyphs.iter().map(|g| g.top()).fold(f32::INFINITY, f32::min);
        glyphs.into_iter().filter(|g| g.top() < top + 6.0).reduce(|a, b| a.union(b))
    });
    let cell = text.width() / ("$ echo hello world".len() as f32 - 0.2);
    let column = |col: f32| text.left() + (col - 0.1) * cell;
    page.long_press(pos2(column(9.5), text.center().y));
    let chip = "document.getElementById('atomics-copy')";
    frame.wait("Copy for the word alone", &format!("!{chip}.hidden && {chip}.title === 'Copy “hello”'"));
    // A handle: a stem along the line and a knob under it, in the accent colour, taller than the line.
    let under = Rect::from_min_max(pos2(cells.left(), text.top() - 6.0), pos2(cells.right(), text.bottom() + 30.0));
    let handles = |page: &mut DevTools| {
        let found = boxes(page, p.accent, 16, under, 30.0);
        let mut found: Vec<Rect> = found.into_iter().filter(|b| b.height() > text.height() + 8.0).collect();
        found.sort_by(|a, b| a.left().total_cmp(&b.left()));
        (found.len() == 2).then_some(found)
    };
    let (found, png) = until(&mut page, "the selection's two handles", handles);
    screens.keep(&png, "word-selected");
    // The end handle's knob is the round end below the line.
    let knob = pos2(found[1].center().x, found[1].bottom() - 7.0);
    page.drag(knob, pos2(column(18.0), knob.y));
    frame.wait("Copy for both words", &format!("{chip}.title === 'Copy “hello world”'"));
    screens.save(&mut page, "words-selected", |_| true);
    frame.eval("window.copied = null; navigator.clipboard.writeText = (text) => { window.copied = text; return Promise.resolve(); }; true");
    let copy = frame.eval(&format!("(() => {{ const r = {chip}.getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2, r.height]; }})()"));
    assert!(number(&copy, 2) >= 44.0, "Copy is a finger's target: {copy}");
    assert!(number(&copy, 1) - number(&copy, 2) / 2.0 >= cells.top(), "Copy leaves the strip's words alone: {copy}");
    page.tap(pos2(number(&copy, 0), number(&copy, 1)));
    frame.wait("the copy done", &format!("window.copied !== null && {chip}.hidden"));
    assert_eq!(frame.eval("window.copied"), json!("hello world"), "Copy copies the selected range and nothing else");
    assert_eq!(typed(&host, "s1"), all, "selecting typed nothing");
    assert_eq!(log.lines.lock().unwrap().iter().filter(|l| l.starts_with("acquire s1")).count(), 1);
    assert!(!log.has("resize s1"), "the phone never resized the terminal");
    assert!(!log.has("upgrade refused"), "every upgrade carried the client's exact origin and the subprotocol");
    assert!(!log.lines.lock().unwrap().iter().any(|l| l.starts_with("host closed")), "no channel was refused");
    eprintln!("phone screens:");
    for path in &screens.saved {
        eprintln!("  {}", path.display());
    }
}

/// A wide window, the size the desktop's QA screenshots are taken at.
const WIDE: (f32, f32) = (1600.0, 1000.0);

#[test]
#[ignore = "needs the built bundle (scripts/build-web.sh) and Chrome"]
fn the_bundle_draws_the_desktops_frame_in_a_wide_window() {
    let dist = dist();
    let mut host = fake_host::host();
    let pairing = host.open_pairing();
    let host = Arc::new(Mutex::new(host));
    let log = Arc::new(Log::default());
    let bind = || TcpListener::bind("127.0.0.1:0").unwrap();
    let (client, parent, tunnel) = (bind(), bind(), bind());
    let port = |l: &TcpListener| l.local_addr().unwrap().port();
    let client_origin = format!("http://127.0.0.1:{}", port(&client));
    let parent_origin = format!("http://localhost:{}", port(&parent));
    let broker_url = format!("ws://127.0.0.1:{}/atomics/tunnel", port(&tunnel));
    let fragment = pairing.fragment();
    let keys: Vec<&str> = fragment.split('.').collect();
    let pair = json!({
        "type": "atomics.embed.boot", "v": 1, "host_id": HOST_ID, "ticket": "ticket-0", "broker": broker_url,
        "display_name": "A Person", "theme": "dark", "scheme": "dark", "locale": "en", "view": "board",
        "mode": "pair", "pair": { "v": 1, "host_noise_pubkey": keys[2], "psk": keys[3] },
    });
    serve_client(client, dist, &parent_origin, port(&tunnel));
    let page_html = include_str!("browser/phone.html")
        .replace("CLIENT_ORIGIN", &client_origin)
        .replace("PHONE_BOOT", &pair.to_string());
    serve_parent(parent, HashMap::from([("/", page_html)]), log.clone(), Arc::new(Mutex::new("go".into())));
    broker(tunnel, host.clone(), log.clone(), client_origin.clone(), Arc::default(), Arc::default());

    let flags = [
        format!("--window-size={},{}", WIDE.0, WIDE.1),
        "--force-device-scale-factor=1".into(),
        "--hide-scrollbars".into(),
    ];
    let browser = launch(&flags, &format!("{parent_origin}/"), &log, OVERALL);
    let devtools = browser.devtools();
    let mut page = DevTools::open(&devtools, "the hosting page", |t| t["type"] == "page");
    let metrics = json!({ "width": WIDE.0 as u32, "height": WIDE.1 as u32, "deviceScaleFactor": 1, "mobile": false });
    page.call("Emulation.setDeviceMetricsOverride", metrics);

    // Paired, and both terminals followed: the agent's tile and the docked chief's.
    log.wait("parent atomics.paired", 1);
    log.wait("hello new", 1);
    log.wait("subscribe s1", 1);
    log.wait("subscribe chief", 1);
    let frame_url = format!("{client_origin}/");
    let mut frame = DevTools::open(&devtools, "the client's frame", |t| {
        t["type"] == "iframe" && t["url"].as_str().is_some_and(|u| u.starts_with(&frame_url))
    });
    frame.wait("the input overlay over a terminal", "!document.getElementById('atomics-input').hidden");

    // The desktop's frame in the boot's theme: the board's Approve plan in the right panel, the chief's prompt in
    // the dock on the left, and the agent's prompt in its tile between them.
    let parent = ParentOrigin::parse(&parent_origin).expect("a loopback origin");
    let Ok(FromParent::Boot(booted)) = boot::read(&parent, &parent_origin, true, &pair) else { panic!("the boot") };
    let p = Palette::new(&booted.theme_tokens(), Fonts::default());
    let dir = target().join("wide-shots");
    std::fs::create_dir_all(&dir).unwrap();
    let ink = Color32::from_rgb(220, 216, 208);
    let approve_area = Rect::from_min_max(pos2(WIDE.0 * 0.6, 34.0), pos2(WIDE.0, 120.0));
    let dock = Rect::from_min_max(pos2(0.0, 60.0), pos2(500.0, 200.0));
    let middle = Rect::from_min_max(pos2(520.0, 60.0), pos2(1030.0, 400.0));
    let (approve, png) = until_on(&mut page, WIDE, &dir, "the frame with the board and both terminals", |page| {
        let terminals = amount(page, ink, 40, dock) > 20.0 && amount(page, ink, 40, middle) > 5.0;
        let button = boxes(page, p.accent, 6, approve_area, 150.0).into_iter().find(|b| b.width() >= 60.0);
        button.filter(|_| terminals)
    });
    std::fs::write(dir.join("browser.png"), base64(&png)).unwrap();
    let desktop = dir.join("desktop.png");
    match std::fs::read(&desktop) {
        Ok(desktop) => {
            let both = page.eval(&format!("sideBySide('{png}', '{}')", encode64(&desktop)));
            let both = both.as_str().expect("the page put the two side by side");
            std::fs::write(dir.join("side-by-side.png"), base64(both)).unwrap();
            eprintln!("wide: {} (this browser, then the desktop)", dir.join("side-by-side.png").display());
        }
        Err(_) => eprintln!(
            "wide: {}; no {} to put beside it (scripts/wide-desktop-shot.sh takes one)",
            dir.join("browser.png").display(),
            desktop.display()
        ),
    }

    // The input lies over the active agent's terminal, between the dock and the board. A click into it types
    // there; a click into the chief's terminal moves the input there, focused, and types there.
    let rect = |frame: &mut DevTools| {
        let r = frame.eval("(() => { const r = document.getElementById('atomics-input').getBoundingClientRect(); return [r.left, r.top, r.width, r.height]; })()");
        let n = |i: usize| r[i].as_f64().expect("a number") as f32;
        Rect::from_min_size(pos2(n(0), n(1)), vec2(n(2), n(3)))
    };
    let agent = rect(&mut frame);
    assert!(agent.left() > 500.0 && agent.right() < 1100.0, "over the agent's tile: {agent:?}");
    let focused = "document.activeElement === document.getElementById('atomics-input')";
    let at = |r: Rect| (f64::from(r.left() + 40.0), f64::from(r.top() + 20.0));
    page.click(at(agent));
    log.wait("acquire s1", 1);
    frame.wait("the focus on the input", focused);
    page.call("Input.insertText", json!({ "text": "ls" }));
    wait_host(&host, "\"ls\" typed into the agent's terminal", |h| h.terminals["s1"].typed == "ls");

    page.click((200.0, 400.0));
    log.wait("acquire chief", 1);
    frame.wait(
        "the input over the chief's terminal, focused",
        &format!("{focused} && document.getElementById('atomics-input').getBoundingClientRect().right < 520"),
    );
    log.wait("release s1", 1);
    page.call("Input.insertText", json!({ "text": "hi" }));
    wait_host(&host, "\"hi\" typed into the chief's terminal", |h| h.terminals["chief"].typed == "hi");
    assert_eq!(typed(&host, "s1"), "ls", "the agent's terminal got nothing more");

    // The board's Approve plan, clicked, approves the plan at its digest.
    page.click((f64::from(approve.center().x), f64::from(approve.center().y)));
    wait_host(&host, "plan_command approve for p1 at its digest", |h| {
        h.received.iter().any(|m| {
            matches!(m, Message::PlanCommand(c) if c.action == PlanAction::Approve
                && c.plan.as_deref() == Some("p1") && c.digest.as_deref() == Some("digest-1"))
        })
    });
    assert!(!log.has("resize s1") && !log.has("resize chief"), "nothing was resized by looking at it");
    assert!(!log.lines.lock().unwrap().iter().any(|l| l.starts_with("host closed")), "no channel was refused");
}

/// The bundle opened and paired over `host`, on a screen of `size`: with a mouse, or as an iPhone by `touch`.
struct Opened {
    host: Arc<Mutex<FakeHost>>,
    log: Arc<Log>,
    _browser: Browser,
    page: DevTools,
    frame: DevTools,
    /// What the client draws with.
    palette: Palette,
    line: Arc<Line>,
}

fn open(mut host: FakeHost, size: (f32, f32), touch: bool) -> Opened {
    let dist = dist();
    let pairing = host.open_pairing();
    let host = Arc::new(Mutex::new(host));
    let log = Arc::new(Log::default());
    let bind = || TcpListener::bind("127.0.0.1:0").unwrap();
    let (client, parent, tunnel) = (bind(), bind(), bind());
    let port = |l: &TcpListener| l.local_addr().unwrap().port();
    let client_origin = format!("http://127.0.0.1:{}", port(&client));
    let parent_origin = format!("http://localhost:{}", port(&parent));
    let broker_url = format!("ws://127.0.0.1:{}/atomics/tunnel", port(&tunnel));
    let fragment = pairing.fragment();
    let keys: Vec<&str> = fragment.split('.').collect();
    let pair = json!({
        "type": "atomics.embed.boot", "v": 1, "host_id": HOST_ID, "ticket": "ticket-0", "broker": broker_url,
        "display_name": "A Person", "theme": "dark", "scheme": "dark", "locale": "en", "view": "board",
        "mode": "pair", "pair": { "v": 1, "host_noise_pubkey": keys[2], "psk": keys[3] },
    });
    serve_client(client, dist, &parent_origin, port(&tunnel));
    let phase = Arc::new(Mutex::new(if touch { "wait" } else { "go" }.to_string()));
    let page_html = include_str!("browser/phone.html")
        .replace("CLIENT_ORIGIN", &client_origin)
        .replace("PHONE_BOOT", &pair.to_string());
    serve_parent(parent, HashMap::from([("/", page_html)]), log.clone(), phase.clone());
    let line = Arc::new(Line::default());
    broker(tunnel, host.clone(), log.clone(), client_origin.clone(), line.clone(), Arc::default());

    let scale = if touch { PHONE_SCALE } else { 1.0 };
    let mut flags = vec![
        format!("--window-size={},{}", size.0, size.1),
        format!("--force-device-scale-factor={scale}"),
        "--hide-scrollbars".to_string(),
    ];
    if touch {
        flags.push(format!("--user-agent={IOS_SAFARI}"));
    }
    let browser = launch(&flags, &format!("{parent_origin}/"), &log, PHONE_OVERALL);
    let devtools = browser.devtools();
    let mut page = DevTools::open(&devtools, "the hosting page", |t| t["type"] == "page");
    let metrics =
        json!({ "width": size.0 as u32, "height": size.1 as u32, "deviceScaleFactor": scale, "mobile": touch });
    page.call("Emulation.setDeviceMetricsOverride", metrics);
    if touch {
        // The client's frame draws in a renderer of its own: its touch screen is set up on its own target, before
        // its first script runs (see the_bundle_works_on_a_phone_by_touch).
        let screen = json!({ "enabled": true, "maxTouchPoints": 5 });
        page.call("Emulation.setTouchEmulationEnabled", screen.clone());
        page.call(
            "Target.setAutoAttach",
            json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
        );
        *phase.lock().unwrap() = "go".into();
        let attached = page.event("Target.attachedToTarget", |e| e["params"]["targetInfo"]["type"] == "iframe");
        let session = attached["params"]["sessionId"].as_str().expect("the frame's session").to_string();
        page.call_in(Some(&session), "Emulation.setTouchEmulationEnabled", screen);
        page.call_in(Some(&session), "Runtime.runIfWaitingForDebugger", json!({}));
    }
    log.wait("parent atomics.paired", 1);
    log.wait("hello new", 1);
    let frame_url = format!("{client_origin}/");
    let frame = DevTools::open(&devtools, "the client's frame", |t| {
        t["type"] == "iframe" && t["url"].as_str().is_some_and(|u| u.starts_with(&frame_url))
    });
    let parent = ParentOrigin::parse(&parent_origin).expect("a loopback origin");
    let Ok(FromParent::Boot(booted)) = boot::read(&parent, &parent_origin, true, &pair) else { panic!("the boot") };
    let palette = Palette::new(&booted.theme_tokens(), Fonts::default());
    Opened { host, log, _browser: browser, page, frame, palette, line }
}

/// The input is on the terminal, and has the focus.
const FOCUSED: &str = "document.activeElement === document.getElementById('atomics-input')";
/// What is cut out of the input.
const CLIP: &str = "document.getElementById('atomics-input').style.clipPath";

/// The screenshots of many plans, for a person to look at.
fn many_plans_dir() -> PathBuf {
    let dir = target().join("many-plans-shots");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
#[ignore = "needs the built bundle (scripts/build-web.sh) and Chrome"]
fn the_input_gives_way_to_a_floating_window() {
    let Opened { host, log, _browser, mut page, mut frame, .. } = open(fake_host::host(), WIDE, false);
    log.wait("subscribe s1", 1);
    log.wait("subscribe chief", 1);
    frame.wait("the input over a terminal", "!document.getElementById('atomics-input').hidden");

    // The docked chief's "float": its window lies over the active agent's terminal, under the input.
    page.click((409.0, 50.0));
    frame.wait("the window cut out of the input", &format!("{CLIP}.startsWith('path(')"));
    let input = frame.eval("(() => { const r = document.getElementById('atomics-input').getBoundingClientRect(); return [r.left, r.right, r.top, r.bottom]; })()");
    let n = |i: usize| input[i].as_f64().expect("a number");
    let (on_window, on_agent) = ((600.0, 500.0), (300.0, 500.0));
    assert!(n(0) < on_agent.0 && on_window.0 < n(1) && n(2) < 500.0 && 500.0 < n(3), "both over the input: {input}");
    // The browser's own hit test: the canvas under the window, the input beside it.
    let hit = |at: (f64, f64)| format!("document.elementFromPoint({}, {}).id", at.0, at.1);
    assert_eq!(frame.eval(&hit(on_window)), json!("atomics"), "the window's part of the input is the canvas's");
    assert_eq!(frame.eval(&hit(on_agent)), json!("atomics-input"));

    // A click on the chief's terminal, and typing: the chief gets the text, and the agent neither it nor a lease.
    page.click(on_window);
    log.wait("acquire chief", 1);
    frame.wait("the focus on the input", FOCUSED);
    page.call("Input.insertText", json!({ "text": "hi" }));
    wait_host(&host, "\"hi\" typed into the chief's terminal", |h| h.terminals["chief"].typed == "hi");
    assert_eq!(typed(&host, "s1"), "", "nothing reached the agent the user was not looking at");
    assert!(!log.has("acquire s1"), "the agent's lease was not taken");

    // Back on the agent's tile, beside the window: the input follows, and the chief's lease is let go.
    page.click(on_agent);
    log.wait("release chief", 1);
    log.wait("acquire s1", 1);
    frame
        .wait("the input back over the agent's terminal, focused", &format!("{FOCUSED} && {CLIP}.startsWith('path(')"));
    page.call("Input.insertText", json!({ "text": "ls" }));
    wait_host(&host, "\"ls\" typed into the agent's terminal", |h| h.terminals["s1"].typed == "ls");
    assert_eq!(typed(&host, "chief"), "hi", "the chief got nothing more");
    assert!(host.lock().unwrap().terminals["chief"].lease.is_none(), "another browser sees the chief free");
    assert!(!log.has("resize s1") && !log.has("resize chief"));
}

#[test]
#[ignore = "needs the built bundle (scripts/build-web.sh) and Chrome"]
fn a_menu_over_a_terminal_acts_and_takes_no_lease() {
    use atomics_core::session::AgentStatus;

    let mut board = fake_host::host();
    board.board.approvals.clear();
    for session in &mut board.board.sessions {
        session.status = AgentStatus::Idle;
        session.pending_approvals = 0;
    }
    let Opened { log, _browser, mut page, mut frame, .. } = open(board, WIDE, false);
    log.wait("subscribe s1", 1);
    frame.wait("the input over a terminal", "!document.getElementById('atomics-input').hidden");

    // "Hide diff": the agent's terminal reaches the right edge, under where the ⋯ menu opens.
    page.click((1316.0, 16.0));
    frame.wait(
        "the terminal under the menu's place",
        "document.getElementById('atomics-input').getBoundingClientRect().right > 1560",
    );
    let (dots, desktop, terminal) = ((1585.0, 16.0), (1542.0, 105.0), (1000.0, 500.0));

    // An entry over the terminal: the menu acts, and the terminal hears nothing of it.
    page.click(dots);
    frame.wait("the open menu taking the input's pointer", &format!("{CLIP} === 'inset(50%)'"));
    assert_eq!(frame.eval(&format!("document.elementFromPoint({}, {}).id", desktop.0, desktop.1)), json!("atomics"));
    page.click(desktop);
    frame.wait("the menu's choice kept", "localStorage.getItem('atomics.layout') === 'desktop'");
    frame.wait("the menu closed", &format!("{CLIP} === ''"));
    assert!(!log.has("acquire s1"), "no lease was taken");
    assert_eq!(frame.eval(FOCUSED), json!(false));

    // A click away closes the menu, as anywhere else; the next click is the terminal's.
    page.click(dots);
    frame.wait("the menu open again", &format!("{CLIP} === 'inset(50%)'"));
    page.click(terminal);
    frame.wait("the menu closed by a click away", &format!("{CLIP} === ''"));
    assert!(!log.has("acquire s1"), "that click was the menu's");
    page.click(terminal);
    log.wait("acquire s1", 1);
    frame.wait("the focus on the input", FOCUSED);
}

#[test]
#[ignore = "needs the built bundle (scripts/build-web.sh) and Chrome"]
fn a_page_of_many_plans_opens_on_the_open_plan() {
    let Opened { host, log, _browser, mut page, palette: p, .. } = open(fake_host::many_plans(), WIDE, false);
    log.wait("subscribe s1", 1);
    log.wait("subscribe chief", 1);
    let dir = many_plans_dir();
    let area = Rect::from_min_max(pos2(WIDE.0 * 0.6, 34.0), pos2(WIDE.0, 120.0));
    let (approve, png) = until_on(&mut page, WIDE, &dir, "Approve plan", |page| {
        boxes(page, p.accent, 6, area, 150.0).into_iter().find(|b| b.width() >= 60.0)
    });
    std::fs::write(dir.join("wide.png"), base64(&png)).unwrap();
    eprintln!("many plans, wide: {}", dir.join("wide.png").display());
    page.click((f64::from(approve.center().x), f64::from(approve.center().y)));
    wait_host(&host, "plan_command approve for the open plan", |h| {
        h.received.iter().any(|m| {
            matches!(m, Message::PlanCommand(c) if c.action == PlanAction::Approve
                && c.plan.as_deref() == Some("p1") && c.digest.as_deref() == Some("digest-1"))
        })
    });
}

#[test]
#[ignore = "needs the built bundle (scripts/build-web.sh) and Chrome"]
fn a_phone_of_many_plans_shows_the_open_plans_board() {
    let Opened { host, _browser, mut page, palette: p, .. } = open(fake_host::many_plans(), PHONE, true);
    let dir = many_plans_dir();
    let pixels = (PHONE.0 * PHONE_SCALE, PHONE.1 * PHONE_SCALE);
    let css = |r: Rect| {
        Rect::from_min_max((r.min.to_vec2() * layout::ZOOM).to_pos2(), (r.max.to_vec2() * layout::ZOOM).to_pos2())
    };
    let points = Rect::from_min_size(Pos2::ZERO, vec2(PHONE.0, PHONE.1) / layout::ZOOM);
    let regions =
        layout::regions(points, &Viewport { visible: Some(points), coarse: true, ..Viewport::default() }, false);
    let (screen, body) = (css(points), css(regions.body));
    let header = Rect::from_min_max(body.left_top(), pos2(screen.right(), body.top() + 270.0));
    let approve = |page: &mut DevTools| {
        boxes(page, p.accent, 6, header, 150.0).into_iter().find(|b| b.width() >= 40.0 && b.height() >= 40.0)
    };

    // The open plan's board at once, its Approve a finger's size, under the one row the picker takes.
    let (button, png) = until_on(&mut page, pixels, &dir, "the open plan's Approve", approve);
    std::fs::write(dir.join("phone.png"), base64(&png)).unwrap();
    eprintln!("many plans, phone: {}", dir.join("phone.png").display());
    assert!(button.top() > body.top() + 44.0 && button.bottom() < body.bottom(), "on screen: {button:?}");

    // History, at the row's right end, opens the finished plans; a tap on one shows its board, read-only.
    let row = body.top() + layout::TOUCH * layout::ZOOM / 2.0;
    page.tap(pos2(screen.right() - 40.0, row));
    thread::sleep(Duration::from_millis(800));
    let shot = page.call("Page.captureScreenshot", json!({ "format": "png" }));
    std::fs::write(dir.join("phone-history.png"), base64(shot["data"].as_str().expect("a screenshot"))).unwrap();
    page.tap(pos2(screen.center().x, row + 70.0));
    let ((), png) =
        until_on(&mut page, pixels, &dir, "the finished plan's board", |page| approve(page).is_none().then_some(()));
    std::fs::write(dir.join("phone-history-plan.png"), base64(&png)).unwrap();

    // The open plan, first in the strip, is one tap away, and a tap on its Approve approves it.
    page.tap(pos2(body.left() + 60.0, row));
    let (button, _) = until_on(&mut page, pixels, &dir, "the open plan's Approve again", approve);
    page.tap(button.center());
    wait_host(&host, "plan_command approve for the open plan", |h| {
        h.received.iter().any(|m| {
            matches!(m, Message::PlanCommand(c) if c.action == PlanAction::Approve
                && c.plan.as_deref() == Some("p1") && c.digest.as_deref() == Some("digest-1"))
        })
    });
}

/// An egui text box has the keyboard: the page's focus is on eframe's own text field.
const TEXT_BOX: &str = "document.activeElement && document.activeElement.tagName === 'INPUT'";
/// The fake host's first agent is still there.
fn has_agent(host: &FakeHost) -> bool {
    host.board.sessions.iter().any(|s| s.id == "s1")
}

/// The screenshots of the browser's actions, for a person to look at.
fn actions_dir() -> PathBuf {
    let dir = target().join("actions-shots");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// What the page shows now, once it drew what was just asked for.
fn keep(page: &mut DevTools, name: &str) {
    thread::sleep(Duration::from_millis(400));
    let shot = page.call("Page.captureScreenshot", json!({ "format": "png" }));
    let out = actions_dir().join(format!("{name}.png"));
    std::fs::write(&out, base64(shot["data"].as_str().expect("a screenshot"))).unwrap();
    eprintln!("actions: {}", out.display());
}

/// The fake host's task `id`.
fn task(host: &FakeHost, id: &str) -> atomics_core::projection::TaskInfo {
    host.board.plans[0].tasks.iter().find(|t| t.id == id).expect("a task").clone()
}

/// The input is on a terminal.
fn terminal_input(frame: &mut DevTools) {
    frame.wait("the input over a terminal", "!document.getElementById('atomics-input').hidden");
}

#[test]
#[ignore = "needs the built bundle (scripts/build-web.sh) and Chrome"]
fn text_boxes_take_the_keyboard_from_a_terminal() {
    let Opened { host, log, _browser, mut page, mut frame, .. } = open(fake_host::working(), WIDE, false);
    log.wait("subscribe s1", 1);
    terminal_input(&mut frame);
    let (terminal, bar) = ((700.0, 500.0), (362.0, 981.0));
    let steered = |host: &Mutex<FakeHost>| host.lock().unwrap().steered.clone();

    // The terminal has the keyboard: what is typed goes there.
    page.click(terminal);
    log.wait("acquire s1", 1);
    frame.wait("the focus on the input", FOCUSED);
    page.call("Input.insertText", json!({ "text": "ls" }));
    wait_host(&host, "\"ls\" typed into the agent's terminal", |h| h.terminals["s1"].typed == "ls");

    // A click on the steer bar takes it, and Return sends what is typed there to the active agent.
    page.click(bar);
    frame.wait("the keyboard on the steer bar", TEXT_BOX);
    assert_eq!(frame.eval(FOCUSED), json!(false));
    page.call("Input.insertText", json!({ "text": "go on" }));
    keep(&mut page, "wide-steer-typed");
    page.key("Enter", 13, Some("\r"));
    wait_host(&host, "the steer", |h| h.steered == [("s1".to_string(), "go on".to_string())]);
    assert_eq!(typed(&host, "s1"), "ls", "nothing reached the terminal");

    // A click back on the terminal types there again, and the bar gets nothing.
    page.click(terminal);
    frame.wait("the focus back on the input", FOCUSED);
    page.call("Input.insertText", json!({ "text": "pwd" }));
    wait_host(&host, "\"pwd\" typed into the agent's terminal", |h| h.terminals["s1"].typed == "lspwd");
    assert_eq!(steered(&host).len(), 1);

    // Send, by a click on it.
    page.click(bar);
    frame.wait("the keyboard on the steer bar again", TEXT_BOX);
    page.call("Input.insertText", json!({ "text": "and again" }));
    thread::sleep(Duration::from_millis(300));
    page.click((1548.0, 982.0));
    wait_host(&host, "the second steer", |h| h.steered.len() == 2 && h.steered[1].1 == "and again");

    // Refused: nothing arrives, the notice says why, and the text is back in the bar to send again.
    host.lock().unwrap().refuse_next =
        Some(WireError::new(ErrorCode::Rejected, "message not delivered: session exited"));
    page.click(bar);
    frame.wait("the keyboard on the steer bar once more", TEXT_BOX);
    page.call("Input.insertText", json!({ "text": "third" }));
    thread::sleep(Duration::from_millis(300));
    page.key("Enter", 13, Some("\r"));
    wait_host(&host, "the refusal used", |h| h.refuse_next.is_none());
    keep(&mut page, "wide-steer-refused");
    assert_eq!(steered(&host).len(), 2);
    page.key("Enter", 13, Some("\r"));
    wait_host(&host, "the text sent again", |h| h.steered.len() == 3 && h.steered[2].1 == "third");
    assert_eq!(typed(&host, "s1"), "lspwd", "none of it reached the terminal");

    // The diff in the right panel, and the terminal with the keyboard again.
    page.click((1456.0, 16.0));
    wait_host(&host, "both files of the diff asked for", |h| {
        let asked = |file: &str| h.received.iter().any(|m| matches!(m, Message::DiffGet { path, .. } if path == file));
        asked("src/a.rs") && asked("src/b.rs")
    });
    keep(&mut page, "wide-diff");
    page.click(terminal);
    frame.wait("the focus back on the input", FOCUSED);

    // A click on a line opens its comment box, which takes the keyboard; Return adds the comment on the host.
    page.click((1374.0, 172.0));
    frame.wait("the keyboard on the comment box", TEXT_BOX);
    page.call("Input.insertText", json!({ "text": "Why this step?" }));
    keep(&mut page, "wide-comment-typed");
    page.key("Enter", 13, Some("\r"));
    wait_host(&host, "the comment", |h| h.board.diffs[0].comments.iter().any(|c| c.body == "Why this step?"));
    assert_eq!(typed(&host, "s1"), "lspwd", "none of it reached the terminal");
    keep(&mut page, "wide-comment");

    // Send review sends it to the agent, as one review, and it leaves the diff.
    page.click((1471.7, 49.5));
    wait_host(&host, "the review", |h| h.reviews.len() == 1 && h.reviews[0].1[0].body == "Why this step?");
    wait_host(&host, "the comment gone from the diff", |h| h.board.diffs[0].comments.is_empty());
    keep(&mut page, "wide-review-sent");
    assert_eq!(typed(&host, "s1"), "lspwd");
    assert!(!log.has("resize s1"));
}

/// Another session of the fake host's, outside its plan.
fn another(board: &mut FakeHost, id: &str, agent: &str, title: &str) {
    let base = board.board.sessions.iter().find(|s| s.id == "s1").expect("the agent").clone();
    board.board.sessions.push(atomics_core::projection::SessionInfo {
        id: id.into(),
        agent: agent.into(),
        title: title.into(),
        task: None,
        worktree: None,
        branch: None,
        cwd: format!("/w/{id}"),
        ..base
    });
    board.terminals.insert(id.into(), fake_host::FakeTerminal::new(80, 12, "$ "));
}

#[test]
#[ignore = "needs the built bundle (scripts/build-web.sh) and Chrome"]
fn a_shell_under_another_name_gets_no_steer_for_want_of_all_agents() {
    let mut board = fake_host::working();
    another(&mut board, "z1", "zsh", "My zsh");
    another(&mut board, "c2", "codex", "Second agent");
    let Opened { host, log, _browser, mut page, mut frame, .. } = open(board, WIDE, false);
    log.wait("subscribe s1", 1);
    terminal_input(&mut frame);
    let (combo, bar) = ((100.0, 983.0), (362.0, 981.0));

    // The row just above Selected: "All agents" where the desktop has it, and the active agent here.
    page.click(combo);
    frame.wait("the list over the input", &format!("{CLIP} === 'inset(50%)'"));
    keep(&mut page, "wide-steer-targets");
    page.click((66.0, 935.0));
    thread::sleep(Duration::from_millis(300));
    page.click(bar);
    frame.wait("the keyboard on the steer bar", TEXT_BOX);
    page.call("Input.insertText", json!({ "text": "rm -rf build" }));
    thread::sleep(Duration::from_millis(300));
    page.key("Enter", 13, Some("\r"));
    wait_host(&host, "the steer", |h| !h.steered.is_empty());
    thread::sleep(Duration::from_millis(400));
    let steered = host.lock().unwrap().steered.clone();
    assert_eq!(steered, [("s1".to_string(), "rm -rf build".to_string())], "the active agent alone");
    assert!(typed(&host, "z1").is_empty(), "nothing reached the shell");
}

#[test]
#[ignore = "needs the built bundle (scripts/build-web.sh) and Chrome"]
fn the_keyboard_in_a_text_box_lets_go_of_the_terminals_lease() {
    let Opened { host, log, _browser, mut page, mut frame, .. } = open(fake_host::working(), WIDE, false);
    log.wait("subscribe s1", 1);
    terminal_input(&mut frame);
    let (terminal, bar) = ((700.0, 500.0), (362.0, 981.0));
    let held = |host: &Mutex<FakeHost>| host.lock().unwrap().terminals["s1"].lease.is_some();

    // A card's open types in its agent, as picking it from the sidebar does.
    page.click((1369.0, 247.0));
    log.wait("acquire s1", 1);
    frame.wait("the focus on the input", FOCUSED);
    page.call("Input.insertText", json!({ "text": "a" }));
    wait_host(&host, "\"a\" typed", |h| h.terminals["s1"].typed == "a");
    assert!(held(&host));

    // Another browser sees the terminal free as soon as the keyboard is in the steer bar.
    page.click(bar);
    frame.wait("the keyboard on the steer bar", TEXT_BOX);
    log.wait("release s1", 1);
    wait_host(&host, "the lease given back", |h| h.terminals["s1"].lease.is_none());
    keep(&mut page, "wide-lease-released");

    // Shift+Enter makes a new line there, as its hint says, and Enter sends.
    page.call("Input.insertText", json!({ "text": "one" }));
    thread::sleep(Duration::from_millis(200));
    for kind in ["keyDown", "keyUp"] {
        let mut key = json!({
            "type": kind, "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "modifiers": 8,
        });
        if kind == "keyDown" {
            key["text"] = json!("\r");
        }
        page.call("Input.dispatchKeyEvent", key);
    }
    thread::sleep(Duration::from_millis(200));
    assert!(host.lock().unwrap().steered.is_empty(), "Shift+Enter sends nothing");
    page.call("Input.insertText", json!({ "text": "two" }));
    keep(&mut page, "wide-steer-two-lines");
    page.key("Enter", 13, Some("\r"));
    wait_host(&host, "the steer of two lines", |h| h.steered == [("s1".to_string(), "one\ntwo".to_string())]);

    // A click back asks again, and types there.
    page.click(terminal);
    log.wait("acquire s1", 2);
    frame.wait("the focus back on the input", FOCUSED);
    page.call("Input.insertText", json!({ "text": "b" }));
    wait_host(&host, "\"b\" typed", |h| h.terminals["s1"].typed == "ab");
    assert!(held(&host));
    keep(&mut page, "wide-lease-asked-again");

    // A comment box is a text box too. The card's open showed the diff.
    wait_host(&host, "both files of the diff asked for", |h| {
        let asked = |file: &str| h.received.iter().any(|m| matches!(m, Message::DiffGet { path, .. } if path == file));
        asked("src/a.rs") && asked("src/b.rs")
    });
    thread::sleep(Duration::from_millis(400));
    page.click(terminal);
    frame.wait("the focus back on the input", FOCUSED);
    wait_host(&host, "the lease again", |h| h.terminals["s1"].lease.is_some());
    page.click((1374.0, 172.0));
    frame.wait("the keyboard on the comment box", TEXT_BOX);
    log.wait("release s1", 2);
    wait_host(&host, "the lease given back again", |h| h.terminals["s1"].lease.is_none());
    assert_eq!(typed(&host, "s1"), "ab");
}

/// The fake host's second task running in a session of its own, beside the first in the plan's tab.
fn a_second_task(board: &mut FakeHost) {
    use atomics_core::plan::TaskStatus;

    another(board, "s2", "claude", "Second task");
    let second = board.board.sessions.iter_mut().find(|s| s.id == "s2").expect("the session");
    second.task = Some(atomics_core::projection::TaskLink { plan: "p1".into(), task: "t2".into() });
    second.worktree = Some("/w/repo-t2".into());
    board.board.plans[0].tasks[1].session = Some("s2".into());
    board.board.plans[0].tasks[1].status = TaskStatus::Running;
}

#[test]
#[ignore = "needs the built bundle (scripts/build-web.sh) and Chrome"]
fn a_steer_one_target_missed_names_it_and_stays_out_of_the_bar() {
    const SIZE: (f32, f32) = (2800.0, 1000.0);
    let mut board = fake_host::working();
    a_second_task(&mut board);
    let Opened { host, log, _browser, mut page, mut frame, .. } = open(board, SIZE, false);
    log.wait("subscribe s1", 1);
    terminal_input(&mut frame);
    let (away, bar) = ((800.0, 16.0), (362.0, 981.0));

    // Both rows in "selected", and the bar's target on them.
    for (row, include) in [((80.5, 81.5), (182.0, 98.0)), ((69.5, 122.0), (171.0, 138.0))] {
        page.right_click(row);
        frame.wait("the menu taking the input's pointer", &format!("{CLIP} === 'inset(50%)'"));
        page.click(include);
        thread::sleep(Duration::from_millis(300));
        page.click(away);
        frame.wait("the menu closed", &format!("{CLIP} === ''"));
    }
    page.click((74.5, 983.5));
    frame.wait("the target's list open", &format!("{CLIP} === 'inset(50%)'"));
    page.click((66.5, 955.5));
    frame.wait("the target's list closed", &format!("{CLIP} === ''"));

    // The first is refused: the other has it, the bar stays empty, and Enter sends nothing twice.
    host.lock().unwrap().refuse_next =
        Some(WireError::new(ErrorCode::Rejected, "message not delivered: session exited"));
    page.click(bar);
    frame.wait("the keyboard on the steer bar", TEXT_BOX);
    page.call("Input.insertText", json!({ "text": "both of you" }));
    thread::sleep(Duration::from_millis(300));
    page.key("Enter", 13, Some("\r"));
    wait_host(&host, "the steer that got through", |h| h.steered == [("s2".to_string(), "both of you".to_string())]);
    keep(&mut page, "wide-steer-one-missed");
    page.key("Enter", 13, Some("\r"));
    thread::sleep(Duration::from_millis(400));
    assert_eq!(host.lock().unwrap().steered.len(), 1, "nothing is left to send twice");
}

#[test]
#[ignore = "needs the built bundle (scripts/build-web.sh) and Chrome"]
fn a_note_takes_the_pointer_and_a_refused_comment_comes_back() {
    let Opened { host, log, _browser, mut page, mut frame, .. } = open(fake_host::working(), WIDE, false);
    log.wait("subscribe s1", 1);
    terminal_input(&mut frame);

    // A long note over the tile's close: the input gives way, and a click on it only dismisses it.
    let long = "message not delivered: the session is not ready for it, and this sentence goes on long enough \
                to take the width the note has and then some, over the terminal, as a real host's message can \
                when it has a good deal to say about what went wrong and what to do about it";
    host.lock().unwrap().refuse_next = Some(WireError::new(ErrorCode::Rejected, long));
    page.click((362.0, 981.0));
    frame.wait("the keyboard on the steer bar", TEXT_BOX);
    page.call("Input.insertText", json!({ "text": "go on" }));
    thread::sleep(Duration::from_millis(300));
    page.key("Enter", 13, Some("\r"));
    wait_host(&host, "the refusal used", |h| h.refuse_next.is_none());
    frame.wait("the note cut out of the input", &format!("{CLIP}.startsWith('path(')"));
    keep(&mut page, "wide-note-long");
    page.click((1023.3, 50.5));
    frame.wait("the note gone, and no dialog", &format!("{CLIP} === ''"));
    keep(&mut page, "wide-note-dismissed");

    // A refused comment comes back in its box, with the keyboard, and Enter tries again.
    page.click((1456.0, 16.0));
    wait_host(&host, "both files of the diff asked for", |h| {
        let asked = |file: &str| h.received.iter().any(|m| matches!(m, Message::DiffGet { path, .. } if path == file));
        asked("src/a.rs") && asked("src/b.rs")
    });
    thread::sleep(Duration::from_millis(400));
    page.click((1374.0, 172.0));
    frame.wait("the keyboard on the comment box", TEXT_BOX);
    page.call("Input.insertText", json!({ "text": "Why this step?" }));
    thread::sleep(Duration::from_millis(300));
    host.lock().unwrap().refuse_next =
        Some(WireError::new(ErrorCode::StaleRevision, "the diff is at revision 5; anchor the comment again"));
    page.key("Enter", 13, Some("\r"));
    wait_host(&host, "the refusal used", |h| h.refuse_next.is_none());
    keep(&mut page, "wide-comment-refused");
    assert!(host.lock().unwrap().board.diffs[0].comments.is_empty());
    frame.wait("the keyboard back on the comment box", TEXT_BOX);
    page.key("Enter", 13, Some("\r"));
    wait_host(&host, "the comment", |h| h.board.diffs[0].comments.iter().any(|c| c.body == "Why this step?"));
    keep(&mut page, "wide-comment-second-try");
}

/// A comment on the fake host's diff, on the line after the first the diff changes.
fn comment_on_the_host(board: &mut FakeHost) {
    board.board.diffs[0].comments.push(atomics_core::steer::ReviewComment {
        path: "src/a.rs".into(),
        line: Some(2),
        removed: false,
        excerpt: "step(1)".into(),
        body: "On the host".into(),
    });
}

/// What this page has sent the host about comments.
fn review_commands(host: &Mutex<FakeHost>) -> usize {
    host.lock().unwrap().received.iter().filter(|m| matches!(m, Message::ReviewCommand(_))).count()
}

#[test]
#[ignore = "needs the built bundle (scripts/build-web.sh) and Chrome"]
fn a_comment_box_keeps_its_draft_while_the_connection_is_down() {
    let mut board = fake_host::working();
    comment_on_the_host(&mut board);
    let Opened { host, log, _browser, mut page, mut frame, line, .. } = open(board, WIDE, false);
    log.wait("subscribe s1", 1);
    terminal_input(&mut frame);
    page.click((1456.0, 16.0));
    wait_host(&host, "both files of the diff asked for", |h| {
        let asked = |file: &str| h.received.iter().any(|m| matches!(m, Message::DiffGet { path, .. } if path == file));
        asked("src/a.rs") && asked("src/b.rs")
    });
    thread::sleep(Duration::from_millis(400));
    page.click((1374.3, 189.7));
    frame.wait("the keyboard on the comment box", TEXT_BOX);
    page.call("Input.insertText", json!({ "text": "draft" }));
    thread::sleep(Duration::from_millis(300));

    line.down.store(true, Ordering::SeqCst);
    line.cut.store(true, Ordering::SeqCst);
    log.wait("cut", 1);
    thread::sleep(Duration::from_millis(1000));
    keep(&mut page, "wide-comment-offline");

    // Enter closes nothing and loses nothing, and what is typed next goes on in the box.
    let before = review_commands(&host);
    for _ in 0..3 {
        page.key("Enter", 13, Some("\r"));
        thread::sleep(Duration::from_millis(150));
    }
    thread::sleep(Duration::from_millis(250));
    assert_eq!(frame.eval(TEXT_BOX), json!(true), "the box keeps the keyboard");
    page.call("Input.insertText", json!({ "text": " more" }));
    keep(&mut page, "wide-comment-offline-enter");

    // A comment on the diff has no × meanwhile.
    page.mouse("mouseMoved", (1196.8, 170.6));
    keep(&mut page, "wide-comment-row-offline");

    // Back: the box still has its text and the keyboard, so Enter sends it without a click.
    line.down.store(false, Ordering::SeqCst);
    log.wait("hello epoch-1", 1);
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(review_commands(&host), before, "nothing was sent while it was down");
    keep(&mut page, "wide-comment-back");
    assert_eq!(frame.eval(TEXT_BOX), json!(true), "the box kept the keyboard");
    page.key("Enter", 13, Some("\r"));
    wait_host(&host, "the comment", |h| h.board.diffs[0].comments.iter().any(|c| c.body == "draft more"));
    assert_eq!(host.lock().unwrap().board.diffs[0].comments.len(), 2, "the other is still there");
    keep(&mut page, "wide-comment-sent-once-back");

    // Down again, a click on a comment changes nothing.
    line.down.store(true, Ordering::SeqCst);
    line.cut.store(true, Ordering::SeqCst);
    log.wait("cut", 2);
    thread::sleep(Duration::from_millis(1000));
    let before = review_commands(&host);
    page.click((1196.8, 170.6));
    keep(&mut page, "wide-comment-row-clicked-offline");
    line.down.store(false, Ordering::SeqCst);
    log.wait("hello epoch-1", 2);
    thread::sleep(Duration::from_millis(1000));
    assert_eq!(review_commands(&host), before);
    assert_eq!(host.lock().unwrap().board.diffs[0].comments.len(), 2);
}

#[test]
#[ignore = "needs the built bundle (scripts/build-web.sh) and Chrome"]
fn a_click_back_within_a_round_trip_types_at_once() {
    let Opened { host, log, _browser, mut page, mut frame, line, .. } = open(fake_host::working(), WIDE, false);
    log.wait("subscribe s1", 1);
    terminal_input(&mut frame);
    let (terminal, bar) = ((700.0, 500.0), (362.0, 981.0));
    const ROUND_TRIP_MS: u64 = 300;
    line.round_trip_ms.store(ROUND_TRIP_MS, Ordering::SeqCst);

    // Back on the terminal before the host has answered the release, and typing at once.
    for gap in [0, 100, 200] {
        page.click(terminal);
        frame.wait("the focus on the input", FOCUSED);
        wait_host(&host, "the lease", |h| h.terminals["s1"].lease.is_some());
        thread::sleep(Duration::from_millis(ROUND_TRIP_MS + 300));
        page.click(bar);
        frame.wait("the keyboard on the steer bar", TEXT_BOX);
        thread::sleep(Duration::from_millis(gap));
        if gap == 100 {
            keep(&mut page, "wide-release-on-its-way");
        }
        page.click(terminal);
        thread::sleep(Duration::from_millis(40));
        let word = format!("[{gap}]");
        page.call("Input.insertText", json!({ "text": word }));
        wait_host(&host, "what was typed", |h| h.terminals["s1"].typed.contains(&word));
        assert_eq!(frame.eval(FOCUSED), json!(true), "one click was enough, {gap} ms after the release");
    }
}

#[test]
#[ignore = "needs the built bundle (scripts/build-web.sh) and Chrome"]
fn a_viewer_has_no_comment_actions_in_a_diff() {
    let mut board = fake_host::working();
    board.answer = Some(atomics_core::noise::Access::View);
    comment_on_the_host(&mut board);
    let Opened { host, log, _browser, mut page, mut frame, .. } = open(board, WIDE, false);
    log.wait("subscribe s1", 1);
    terminal_input(&mut frame);
    page.click((1372.0, 16.0));
    wait_host(&host, "both files of the diff asked for", |h| {
        let asked = |file: &str| h.received.iter().any(|m| matches!(m, Message::DiffGet { path, .. } if path == file));
        asked("src/a.rs") && asked("src/b.rs")
    });
    thread::sleep(Duration::from_millis(400));
    keep(&mut page, "viewer-diff");

    page.mouse("mouseMoved", (1196.8, 170.6));
    keep(&mut page, "viewer-comment-hover");
    page.click((1196.8, 170.6));
    keep(&mut page, "viewer-comment-clicked");
    page.click((1374.3, 189.7));
    thread::sleep(Duration::from_millis(400));
    assert_eq!(frame.eval(TEXT_BOX), json!(false), "a viewer never gets a box");
    keep(&mut page, "viewer-line-clicked");
    assert_eq!(review_commands(&host), 0);
    assert_eq!(host.lock().unwrap().board.diffs[0].comments.len(), 1);
}

#[test]
#[ignore = "needs the built bundle (scripts/build-web.sh) and Chrome"]
fn a_finished_plan_picked_on_a_phone_shows_in_the_strip() {
    use atomics_core::plan::PlanStatus;

    let mut board = fake_host::many_plans();
    for plan in board.board.plans.iter_mut().filter(|plan| ("a-01".."a-08").contains(&plan.id.as_str())) {
        plan.status = PlanStatus::Open;
    }
    let Opened { _browser, mut page, .. } = open(board, PHONE, true);
    let css = |r: Rect| {
        Rect::from_min_max((r.min.to_vec2() * layout::ZOOM).to_pos2(), (r.max.to_vec2() * layout::ZOOM).to_pos2())
    };
    let points = Rect::from_min_size(Pos2::ZERO, vec2(PHONE.0, PHONE.1) / layout::ZOOM);
    let regions =
        layout::regions(points, &Viewport { visible: Some(points), coarse: true, ..Viewport::default() }, false);
    let (screen, body) = (css(points), css(regions.body));
    let row = body.top() + layout::TOUCH * layout::ZOOM / 2.0;
    thread::sleep(Duration::from_millis(800));
    keep(&mut page, "phone-strip");
    page.tap(pos2(screen.right() - 40.0, row));
    thread::sleep(Duration::from_millis(800));
    keep(&mut page, "phone-strip-history");
    page.tap(pos2(screen.center().x, row + 70.0));
    thread::sleep(Duration::from_millis(800));
    keep(&mut page, "phone-strip-history-picked");

    // The plan before it is still a tap away, at the strip's left end.
    page.tap(pos2(body.left() + 40.0, row));
    thread::sleep(Duration::from_millis(800));
    keep(&mut page, "phone-strip-tap-before");
}

#[test]
#[ignore = "needs the built bundle (scripts/build-web.sh) and Chrome"]
fn a_board_and_a_tile_act_in_a_wide_window() {
    use atomics_core::plan::TaskStatus;

    let mut board = fake_host::working();
    let mut another = board.board.plans[0].tasks[2].clone();
    (another.id, another.title) = ("t7".into(), "Task t7".into());
    board.board.plans[0].tasks.push(another);
    let Opened { host, log, _browser, mut page, mut frame, .. } = open(board, WIDE, false);
    log.wait("subscribe s1", 1);
    terminal_input(&mut frame);
    keep(&mut page, "wide-board");

    // A card's start starts that task; Dispatch ready, then, the other one that is ready.
    page.click((1086.5, 304.0));
    wait_host(&host, "t3 started", |h| task(h, "t3").status == TaskStatus::Running);
    assert_eq!(task(&host.lock().unwrap(), "t7").status, TaskStatus::Planned);
    thread::sleep(Duration::from_millis(300));
    page.click((1415.0, 52.5));
    wait_host(&host, "t7 dispatched", |h| task(h, "t7").status == TaskStatus::Running);
    keep(&mut page, "wide-board-dispatched");

    // A running agent's × asks, over the terminal, whose input gives way to the dialog; Cancel closes nothing.
    let (close, confirm, cancel) = ((1023.3, 50.5), (690.5, 532.0), (780.4, 532.0));
    page.click(close);
    frame.wait("the dialog cut out of the input", &format!("{CLIP}.startsWith('path(')"));
    keep(&mut page, "wide-close-session");
    assert!(has_agent(&host.lock().unwrap()), "not yet");
    page.click(cancel);
    frame.wait("the dialog gone", &format!("{CLIP} === ''"));
    assert!(has_agent(&host.lock().unwrap()));

    // The dialog's Close session closes it, and the input goes with its terminal.
    page.click(close);
    frame.wait("the dialog again", &format!("{CLIP}.startsWith('path(')"));
    page.click(confirm);
    wait_host(&host, "the agent closed", |h| !has_agent(h));
    frame.wait("the input gone with its terminal", "document.getElementById('atomics-input').hidden");
    keep(&mut page, "wide-session-closed");
    assert!(!log.lines.lock().unwrap().iter().any(|l| l.starts_with("host closed")), "no channel was refused");
}

#[test]
#[ignore = "needs the built bundle (scripts/build-web.sh) and Chrome"]
fn a_plan_closes_and_a_new_session_starts() {
    use atomics_core::plan::PlanStatus;

    let Opened { host, log, _browser, mut page, mut frame, .. } = open(fake_host::working(), WIDE, false);
    log.wait("subscribe s1", 1);
    terminal_input(&mut frame);

    // + New's picker: what the mirror names, started in a scratch folder, on the Ungrouped tab.
    page.click((1388.5, 16.0));
    keep(&mut page, "wide-new-session");
    page.click((700.6, 545.0));
    keep(&mut page, "wide-new-session-cancelled");
    assert_eq!(host.lock().unwrap().board.sessions.len(), 2, "Cancel starts nothing");
    // Refused: it says why, and the page stays on the plan.
    page.click((1388.5, 16.0));
    thread::sleep(Duration::from_millis(400));
    host.lock().unwrap().refuse_next = Some(WireError::new(ErrorCode::NotFound, "no agent claude is configured"));
    page.click((640.0, 545.0));
    wait_host(&host, "the refusal used", |h| h.refuse_next.is_none());
    keep(&mut page, "wide-new-session-refused");
    assert_eq!(host.lock().unwrap().board.sessions.len(), 2);
    page.click((1388.5, 16.0));
    thread::sleep(Duration::from_millis(400));
    page.click((640.0, 545.0));
    wait_host(&host, "a new session", |h| h.board.sessions.iter().any(|s| s.id.starts_with("new-")));
    keep(&mut page, "wide-new-session-started");
    assert_eq!(
        host.lock().unwrap().board.sessions.iter().find(|s| s.id.starts_with("new-")).unwrap().cwd,
        "/scratch/new"
    );
    log.wait("subscribe new-0", 1);

    // Back on the plan, its board: Close plan… offers Archive alone, and Cancel keeps it open.
    page.click((372.5, 16.5));
    thread::sleep(Duration::from_millis(300));
    page.click((1456.0, 16.0));
    thread::sleep(Duration::from_millis(300));
    let (close, archive, cancel) = ((1541.0, 52.5), (687.0, 533.5), (773.2, 532.0));
    page.click(close);
    keep(&mut page, "wide-close-plan");
    page.click(cancel);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(host.lock().unwrap().board.plans[0].status, PlanStatus::Open);
    page.click(close);
    thread::sleep(Duration::from_millis(400));
    page.click(archive);
    wait_host(&host, "the plan archived", |h| h.board.plans[0].status == PlanStatus::Archived);
    keep(&mut page, "wide-plan-archived");
    assert!(!log.lines.lock().unwrap().iter().any(|l| l.starts_with("host closed")), "no channel was refused");
}

#[test]
#[ignore = "needs the built bundle (scripts/build-web.sh) and Chrome"]
fn a_row_steers_and_a_review_column_acts() {
    use atomics_core::plan::TaskStatus;

    // A window with room for the sidebar beside the docked chief.
    const SIZE: (f32, f32) = (2800.0, 1000.0);
    let Opened { host, log, _browser, mut page, mut frame, .. } = open(fake_host::working(), SIZE, false);
    log.wait("subscribe s1", 1);
    terminal_input(&mut frame);

    // A row's menu puts it in "selected", and the bar's target sends to the selected.
    page.right_click((80.5, 81.5));
    frame.wait("the menu taking the input's pointer", &format!("{CLIP} === 'inset(50%)'"));
    keep(&mut page, "wide-row-menu");
    page.click((182.0, 98.0));
    thread::sleep(Duration::from_millis(300));
    page.click((1500.0, 700.0));
    frame.wait("the menu closed", &format!("{CLIP} === ''"));
    page.click((74.5, 983.5));
    frame.wait("the target's list open", &format!("{CLIP} === 'inset(50%)'"));
    keep(&mut page, "wide-steer-targets");
    page.click((66.5, 955.5));
    frame.wait("the target's list closed", &format!("{CLIP} === ''"));
    page.click((362.0, 981.0));
    frame.wait("the keyboard on the steer bar", TEXT_BOX);
    page.call("Input.insertText", json!({ "text": "both of you" }));
    keep(&mut page, "wide-steer-selected");
    page.key("Enter", 13, Some("\r"));
    wait_host(&host, "the steer to the selected", |h| h.steered == [("s1".to_string(), "both of you".to_string())]);

    // The divider dragged left, as far as the agent's tile lets it, brings the board's fourth column on screen.
    page.mouse_drag((1877.0, 600.0), (1000.0, 600.0));
    keep(&mut page, "wide-board-wider");
    page.click((2265.0, 227.0));
    wait_host(&host, "t6 unblocked", |h| task(h, "t6").status == TaskStatus::Running);
    thread::sleep(Duration::from_millis(300));
    page.click((2709.4, 228.5));
    wait_host(&host, "t4 integrated", |h| task(h, "t4").integrated.is_some());
    thread::sleep(Duration::from_millis(300));
    page.click((2547.5, 228.5));
    wait_host(&host, "t4 accepted", |h| task(h, "t4").status == TaskStatus::Done);
    thread::sleep(Duration::from_millis(300));
    page.click((1948.9, 112.5));
    wait_host(&host, "t5 integrated", |h| task(h, "t5").integrated.is_some());
    keep(&mut page, "wide-board-integrated");

    // A panel this wide has the changed-files column beside the diff; a click there scrolls to the file, and
    // asks nothing of the host.
    page.click((2656.2, 16.0));
    wait_host(&host, "both files of the diff asked for", |h| {
        let asked = |file: &str| h.received.iter().any(|m| matches!(m, Message::DiffGet { path, .. } if path == file));
        asked("src/a.rs") && asked("src/b.rs")
    });
    keep(&mut page, "wide-changed-files");
    let before = host.lock().unwrap().received.len();
    page.click((1720.0, 124.0));
    keep(&mut page, "wide-changed-files-picked");
    assert_eq!(host.lock().unwrap().received.len(), before, "nothing went to the host");
}

#[test]
#[ignore = "needs the built bundle (scripts/build-web.sh) and Chrome"]
fn a_phone_acts_by_touch() {
    use atomics_core::plan::{PlanStatus, TaskStatus};

    // A review and an accepted task, on the narrowest screen; the board's other columns are the wide layout's test's.
    const SIZE: (f32, f32) = (360.0, 740.0);
    let at = |x: f32, y: f32| pos2(x * layout::ZOOM, y * layout::ZOOM);
    let mut board = fake_host::working();
    board.board.plans[0].tasks.retain(|t| t.id == "t4" || t.id == "t5");
    let Opened { host, log, _browser, mut page, mut frame, .. } = open(board, SIZE, true);
    log.wait("hello new", 1);
    frame.wait("an iPhone's screen in the frame", "matchMedia('(pointer: coarse)').matches");
    thread::sleep(Duration::from_millis(600));
    keep(&mut page, "phone-board");

    // The board's buttons, each a tap.
    page.tap(at(210.6, 413.4));
    wait_host(&host, "t4 integrated", |h| task(h, "t4").integrated.is_some());
    thread::sleep(Duration::from_millis(300));
    page.tap(at(51.0, 413.8));
    wait_host(&host, "t4 accepted", |h| task(h, "t4").status == TaskStatus::Done);
    keep(&mut page, "phone-accepted");
    page.tap(at(50.0, 273.9));
    wait_host(&host, "t5 integrated", |h| task(h, "t5").integrated.is_some());
    keep(&mut page, "phone-integrated");

    // Close plan… offers Archive alone, at a finger's size, and Cancel keeps it open.
    page.tap(at(58.4, 191.1));
    keep(&mut page, "phone-close-plan");
    page.tap(at(151.1, 329.6));
    thread::sleep(Duration::from_millis(400));
    assert_eq!(host.lock().unwrap().board.plans[0].status, PlanStatus::Open);

    // A terminal takes the keyboard by a tap; going on to the diff takes it from the terminal.
    let (terminal_pane, diffs_pane) = (at(201.6, 569.2), at(144.5, 569.2));
    page.tap(terminal_pane);
    log.wait("subscribe s1", 1);
    terminal_input(&mut frame);
    keep(&mut page, "phone-terminal");
    page.tap(at(100.0, 300.0));
    log.wait("acquire s1", 1);
    frame.wait("the focus on the input after a tap", FOCUSED);
    page.call("Input.insertText", json!({ "text": "ls" }));
    wait_host(&host, "\"ls\" typed into the agent's terminal", |h| h.terminals["s1"].typed == "ls");
    // The key bar takes the pane bar's place; a tap on the terminal's own header moves on.
    frame.eval("document.getElementById('atomics-input').blur()");
    frame.wait("the key bar gone", "document.getElementById('atomics-keybar').hidden");
    thread::sleep(Duration::from_millis(300));
    page.tap(diffs_pane);
    frame.wait("no terminal on the diffs pane", "document.getElementById('atomics-input').hidden");
    thread::sleep(Duration::from_millis(300));
    page.tap(at(45.4, 67.8));
    wait_host(&host, "both files of the diff asked for", |h| {
        let asked = |file: &str| h.received.iter().any(|m| matches!(m, Message::DiffGet { path, .. } if path == file));
        asked("src/a.rs") && asked("src/b.rs")
    });
    keep(&mut page, "phone-diff");

    // A tap on a line opens its comment box, which takes the keyboard; Return adds the comment on the host.
    page.tap(at(100.0, 292.6));
    frame.wait("the keyboard on the comment box", TEXT_BOX);
    page.call("Input.insertText", json!({ "text": "Why this step?" }));
    keep(&mut page, "phone-comment-typed");
    page.key("Enter", 13, Some("\r"));
    wait_host(&host, "the comment", |h| h.board.diffs[0].comments.iter().any(|c| c.body == "Why this step?"));
    assert_eq!(typed(&host, "s1"), "ls", "none of it reached the terminal");
    keep(&mut page, "phone-comment");
    page.tap(at(110.0, 143.2));
    wait_host(&host, "the review", |h| h.reviews.len() == 1 && h.reviews[0].1[0].body == "Why this step?");
    keep(&mut page, "phone-review-sent");

    // A tile's ×, a finger's size, asks; Cancel closes nothing, and the dialog's Close session closes it.
    page.tap(terminal_pane);
    terminal_input(&mut frame);
    thread::sleep(Duration::from_millis(400));
    let close = at(SIZE.0 / layout::ZOOM - 22.8, 60.4);
    page.tap(close);
    frame.wait("the dialog cut out of the input", &format!("{CLIP}.startsWith('path(')"));
    keep(&mut page, "phone-close-session");
    assert!(has_agent(&host.lock().unwrap()), "not yet");
    page.tap(at(156.5, 329.6));
    frame.wait("the dialog gone", &format!("{CLIP} === ''"));
    assert!(has_agent(&host.lock().unwrap()));
    page.tap(close);
    frame.wait("the dialog again", &format!("{CLIP}.startsWith('path(')"));
    page.tap(at(71.0, 329.6));
    wait_host(&host, "the agent closed", |h| !has_agent(h));
    keep(&mut page, "phone-session-closed");

    // Archive, from the board.
    page.tap(at(86.3, 569.2));
    thread::sleep(Duration::from_millis(400));
    page.tap(at(58.4, 191.1));
    keep(&mut page, "phone-close-plan-again");
    page.tap(at(68.2, 330.8));
    wait_host(&host, "the plan archived", |h| h.board.plans[0].status == PlanStatus::Archived);
    keep(&mut page, "phone-plan-archived");

    // It was the only plan: History reaches it, read-only, and looking changes nothing on the host.
    let before = host.lock().unwrap().received.len();
    let row = layout::px(52.0) + layout::TOUCH / 2.0;
    page.tap(at(SIZE.0 / layout::ZOOM - 32.0, row));
    keep(&mut page, "phone-history");
    page.tap(at(SIZE.0 / layout::ZOOM / 2.0, row + 56.0));
    keep(&mut page, "phone-history-plan");
    assert_eq!(host.lock().unwrap().received.len(), before, "nothing went to the host");
    assert!(!log.lines.lock().unwrap().iter().any(|l| l.starts_with("host closed")), "no channel was refused");
}
