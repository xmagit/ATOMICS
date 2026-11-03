//! The browser glue: eframe's web runner, the window's message events, the broker's WebSocket and the
//! device store, each turned into the driver's inputs, and the driver's outputs carried out. Nothing is
//! decided here; the driver decides, and it is tested natively.
//!
//! The parent origin comes from the hosting page's config script (`config.js` beside the bundle sets
//! `window.ATOMICS_PARENT_ORIGIN`). A message from any other origin, or from a window other than the one
//! that framed this page, is dropped before its data is read.
//!
//! Terminal input is the page's `<textarea>` overlay and key bar (`web/terminal.js`): what it reports is
//! read by [`crate::page`] and handed to the terminal pane, and after each frame the overlay is placed and
//! set as the pane says ([`Overlay`]).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Duration;

use atomics_core::noise::{KEY_BYTES, PublicKey, Secret};
use atomics_core::tunnel;
use atomics_ui::Client;
use atomics_ui::theme::Palette;
use js_sys::{Array, ArrayBuffer, JSON, Object, Reflect, Uint8Array};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{BinaryType, CloseEvent, HtmlCanvasElement, MessageEvent, WebSocket, Window};
use zeroize::Zeroizing;

use crate::boot::{self, ParentOrigin};
use crate::device::{self, Device};
use crate::driver::{Driver, Output};
use crate::layout::{self, LayoutMode};
use crate::page::{self, PageEvent};
use crate::term_pane::Overlay;
use crate::views::{self, BundledFonts, Tab, View};

/// The largest message event read from the parent, as JSON text.
const MAX_MESSAGE_BYTES: usize = 64 * 1024;

#[wasm_bindgen(module = "/web/device-store.js")]
extern "C" {
    #[wasm_bindgen(catch, js_name = loadDevice)]
    async fn load_device(host_id: &str) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(catch, js_name = saveDevice)]
    async fn save_device(host_id: &str, host_key: &str, secret: &[u8]) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(catch, js_name = forgetDevice)]
    async fn forget_device(host_id: &str) -> Result<JsValue, JsValue>;
}

#[wasm_bindgen(module = "/web/terminal.js")]
extern "C" {
    fn install(keys: &str, callback: &Closure<dyn FnMut(JsValue)>);
    fn place(x: f32, y: f32, width: f32, height: f32);
    fn hide();
    #[wasm_bindgen(js_name = setWritable)]
    fn set_writable(on: bool);
    #[wasm_bindgen(js_name = blurInput)]
    fn blur_input();
    #[wasm_bindgen(js_name = focusInput)]
    fn focus_input();
    #[wasm_bindgen(js_name = setSticky)]
    fn set_sticky(ctrl: bool, alt: bool);
    #[wasm_bindgen(js_name = setCopy)]
    fn set_copy(text: Option<String>, x: f32, y: f32);
    #[wasm_bindgen(js_name = setTheme)]
    fn set_theme(colors: &str);
    #[wasm_bindgen(js_name = loadLayout)]
    fn load_layout() -> String;
    #[wasm_bindgen(js_name = saveLayout)]
    fn save_layout(name: &str);
}

/// The page's own controls (the key bar, the Copy chip) in the theme's colours.
fn theme_colors(p: &Palette) -> String {
    let css = |c: egui::Color32| {
        let [r, g, b, a] = c.to_srgba_unmultiplied();
        format!("rgba({r}, {g}, {b}, {:.3})", f32::from(a) / 255.0)
    };
    serde_json::json!({
        "bar": css(p.topbar),
        "key": css(p.raised),
        "key-text": css(p.foreground),
        "border": css(p.border),
        "accent": css(p.accent),
        "on-accent": css(p.on_accent),
    })
    .to_string()
}

/// A clock for gestures: the page's, which its events are stamped with.
fn page_now() -> f64 {
    web_sys::window().and_then(|w| w.performance()).map_or(0.0, |p| p.now())
}

/// What happened in the browser since the last frame.
enum Input {
    Message { origin: String, data: serde_json::Value },
    Open { socket: u64, protocol: String },
    Data { socket: u64, bytes: Vec<u8> },
    Closed { socket: u64, code: u16 },
    Device(Option<Device>),
    Note(&'static str),
    Page(PageEvent),
}

type Inbox = Rc<RefCell<VecDeque<Input>>>;

fn push(inbox: &Inbox, ctx: &egui::Context, input: Input) {
    inbox.borrow_mut().push_back(input);
    ctx.request_repaint();
}

/// The broker's socket, with the handlers that feed the inbox. Dropping it closes it normally.
struct Live {
    id: u64,
    ws: WebSocket,
    _open: Closure<dyn FnMut()>,
    _message: Closure<dyn FnMut(MessageEvent)>,
    _close: Closure<dyn FnMut(CloseEvent)>,
}

impl Live {
    /// Detach the handlers, so nothing from this socket reaches the driver again and no event finds
    /// its closure dropped, then close it with `code` unless it is closing already.
    fn close(&self, code: u16) {
        self.ws.set_onopen(None);
        self.ws.set_onmessage(None);
        self.ws.set_onclose(None);
        if self.ws.ready_state() < WebSocket::CLOSING {
            let _ = self.ws.close_with_code(code);
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.close(tunnel::close::NORMAL);
    }
}

struct App {
    driver: Driver,
    client: Client,
    view: View,
    inbox: Inbox,
    ctx: egui::Context,
    parent: Option<(Window, ParentOrigin)>,
    socket: Option<Live>,
    themed: bool,
    _listener: Closure<dyn FnMut(MessageEvent)>,
    _page: Closure<dyn FnMut(JsValue)>,
    /// What the overlay was last set to.
    applied: Overlay,
    /// The layout last kept in the page's storage.
    saved_layout: LayoutMode,
}

/// Start the client on the page's `#atomics` canvas.
pub fn start() {
    std::panic::set_hook(Box::new(|info| web_sys::console::error_1(&JsValue::from_str(&info.to_string()))));
    wasm_bindgen_futures::spawn_local(async {
        let Some(window) = web_sys::window() else { return };
        let Some(document) = window.document() else { return };
        let canvas = document.get_element_by_id("atomics").and_then(|e| e.dyn_into::<HtmlCanvasElement>().ok());
        let Some(canvas) = canvas else { return };
        let started = eframe::WebRunner::new()
            .start(canvas, eframe::WebOptions::default(), Box::new(|cc| Ok(Box::new(App::new(cc)))))
            .await;
        if started.is_err()
            && let Some(error) = document.get_element_by_id("atomics-error")
        {
            error.set_text_content(Some("ATOMICS could not start here: this browser needs WebGL 2."));
            let _ = error.remove_attribute("hidden");
        }
    });
}

/// A random prefix for this page's operation IDs.
fn op_prefix() -> String {
    let mut bytes = [0u8; 8];
    let _ = getrandom::fill(&mut bytes);
    format!("web-{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

fn json(value: &serde_json::Value) -> JsValue {
    JSON::parse(&value.to_string()).unwrap_or(JsValue::NULL)
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = cc.egui_ctx.clone();
        let window = web_sys::window().expect("a window");
        // Framed, by a window other than this one, and told which origin that is.
        let framer = window.parent().ok().flatten().filter(|p| !Object::is(p.as_ref(), window.as_ref()));
        let configured = Reflect::get(window.as_ref(), &JsValue::from_str(boot::PARENT_ORIGIN_GLOBAL))
            .ok()
            .and_then(|value| value.as_string());
        let parent = framer.zip(configured.as_deref().and_then(ParentOrigin::parse));
        let agent = window.navigator().user_agent().unwrap_or_default();
        let driver =
            Driver::new(parent.as_ref().map(|(_, origin)| origin.clone()), &op_prefix(), &device::name(&agent));
        let client = Client::default();
        client.palette.apply(&ctx, &BundledFonts);
        let inbox: Inbox = Rc::default();
        let listener = {
            let (inbox, ctx, parent) = (inbox.clone(), ctx.clone(), parent.clone());
            Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                let Some((framer, origin)) = &parent else { return };
                if !origin.sent(&event.origin()) {
                    return;
                }
                let from_parent = event.source().is_some_and(|source| Object::is(&source, framer.as_ref()));
                if !from_parent {
                    return;
                }
                let Ok(text) = JSON::stringify(&event.data()) else { return };
                let text = String::from(text);
                if text.len() > MAX_MESSAGE_BYTES {
                    return;
                }
                if let Ok(data) = serde_json::from_str(&text) {
                    push(&inbox, &ctx, Input::Message { origin: event.origin(), data });
                }
            })
        };
        let _ = window.add_event_listener_with_callback("message", listener.as_ref().unchecked_ref());
        let page = {
            let (inbox, ctx) = (inbox.clone(), ctx.clone());
            Closure::<dyn FnMut(JsValue)>::new(move |event: JsValue| {
                let Ok(text) = JSON::stringify(&event) else { return };
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&String::from(text)) else { return };
                if let Some(event) = page::read(&value, ctx.zoom_factor()) {
                    push(&inbox, &ctx, Input::Page(event));
                }
            })
        };
        install(&layout::keybar_json().to_string(), &page);
        set_theme(&theme_colors(&client.palette));
        let layout = LayoutMode::named(&load_layout());
        let mut view = View::default();
        view.layout = layout;
        Self {
            driver,
            client,
            view,
            inbox,
            ctx,
            parent,
            socket: None,
            themed: false,
            _listener: listener,
            _page: page,
            applied: Overlay::default(),
            saved_layout: layout,
        }
    }

    /// Place and set the overlay as the terminal pane says, telling the page only what changed.
    fn apply_overlay(&mut self) {
        let zoom = self.ctx.zoom_factor();
        let overlay = &mut self.view.term.overlay;
        if std::mem::take(&mut overlay.blur) {
            blur_input();
        }
        let focus = std::mem::take(&mut overlay.focus);
        let now = overlay.clone();
        let last = &self.applied;
        if now.writable != last.writable {
            set_writable(now.writable);
        }
        if now.rect != last.rect {
            match now.rect {
                Some(r) => place(r.left() * zoom, r.top() * zoom, r.width() * zoom, r.height() * zoom),
                None => hide(),
            }
        }
        if (now.ctrl, now.alt) != (last.ctrl, last.alt) {
            set_sticky(now.ctrl, now.alt);
        }
        // After placing it: a click picked this terminal, so typing goes there.
        if focus {
            focus_input();
        }
        if now.copy != last.copy {
            match &now.copy {
                Some((text, at)) => set_copy(Some(text.clone()), at.x * zoom, at.y * zoom),
                None => set_copy(None, 0.0, 0.0),
            }
        }
        self.applied = now;
    }

    fn post(&self, kind: &str) {
        if let Some((framer, origin)) = &self.parent {
            let _ = framer.post_message(&json(&boot::say(kind)), origin.as_str());
        }
    }

    fn dial(&mut self, socket: u64, url: &str, protocols: &[Zeroizing<String>]) {
        self.socket = None;
        let offered = Array::new();
        for protocol in protocols {
            offered.push(&JsValue::from_str(protocol));
        }
        let Ok(ws) = WebSocket::new_with_str_sequence(url, &offered) else {
            return push(&self.inbox, &self.ctx, Input::Closed { socket, code: 1006 });
        };
        ws.set_binary_type(BinaryType::Arraybuffer);
        let (inbox, ctx) = (self.inbox.clone(), self.ctx.clone());
        let opened = ws.clone();
        let open = Closure::<dyn FnMut()>::new(move || {
            push(&inbox, &ctx, Input::Open { socket, protocol: opened.protocol() });
        });
        let (inbox, ctx) = (self.inbox.clone(), self.ctx.clone());
        let message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            // The broker sends binary messages only; anything else is not this protocol.
            if let Ok(buffer) = event.data().dyn_into::<ArrayBuffer>() {
                push(&inbox, &ctx, Input::Data { socket, bytes: Uint8Array::new(&buffer).to_vec() });
            }
        });
        let (inbox, ctx) = (self.inbox.clone(), self.ctx.clone());
        let close = Closure::<dyn FnMut(CloseEvent)>::new(move |event: CloseEvent| {
            push(&inbox, &ctx, Input::Closed { socket, code: event.code() });
        });
        ws.set_onopen(Some(open.as_ref().unchecked_ref()));
        ws.set_onmessage(Some(message.as_ref().unchecked_ref()));
        ws.set_onclose(Some(close.as_ref().unchecked_ref()));
        self.socket = Some(Live { id: socket, ws, _open: open, _message: message, _close: close });
    }

    fn run(&mut self, output: Output) {
        match output {
            Output::Post(kind) => self.post(kind),
            Output::Dial { socket, url, protocols } => self.dial(socket, &url, &protocols),
            Output::Send { socket, message } => {
                if let Some(live) = self.socket.as_ref().filter(|live| live.id == socket) {
                    let _ = live.ws.send_with_u8_array(&message);
                }
            }
            Output::Close { socket, code } => {
                if let Some(live) = self.socket.take_if(|live| live.id == socket) {
                    live.close(code);
                }
            }
            Output::LoadDevice(host) => {
                let (inbox, ctx) = (self.inbox.clone(), self.ctx.clone());
                wasm_bindgen_futures::spawn_local(async move {
                    let device = load_device(&host).await.ok().and_then(|stored| stored_device(&host, &stored));
                    push(&inbox, &ctx, Input::Device(device));
                });
            }
            Output::SaveDevice(device) => {
                let (inbox, ctx) = (self.inbox.clone(), self.ctx.clone());
                wasm_bindgen_futures::spawn_local(async move {
                    let key = device.host_key.text();
                    if save_device(&device.host_id, &key, device.secret.as_bytes()).await.is_err() {
                        let note = "This browser could not keep its key, so it will need pairing again next time.";
                        push(&inbox, &ctx, Input::Note(note));
                    }
                });
            }
            Output::ForgetDevice(host) => {
                wasm_bindgen_futures::spawn_local(async move {
                    let _ = forget_device(&host).await;
                });
            }
        }
    }

    fn run_outputs(&mut self) {
        loop {
            let outputs = self.driver.take_outputs();
            if outputs.is_empty() {
                return;
            }
            for output in outputs {
                self.run(output);
            }
        }
    }
}

/// A stored device as `device-store.js` hands it back: `{hostKey, secret}`, or null.
fn stored_device(host_id: &str, stored: &JsValue) -> Option<Device> {
    if stored.is_null() || stored.is_undefined() {
        return None;
    }
    let host_key = Reflect::get(stored, &JsValue::from_str("hostKey")).ok()?.as_string()?;
    let secret = Reflect::get(stored, &JsValue::from_str("secret")).ok()?.dyn_into::<Uint8Array>().ok()?;
    let mut bytes = Zeroizing::new([0u8; KEY_BYTES]);
    let read = secret.length() as usize == KEY_BYTES;
    if read {
        secret.copy_to(&mut bytes[..]);
    }
    secret.fill(0, 0, secret.length());
    let host_key = PublicKey::parse(&host_key)?;
    read.then(|| Device { host_id: host_id.to_string(), host_key, secret: Secret::from_bytes(&bytes) })
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.driver.tick(js_sys::Date::now() as u64);
        let inputs: Vec<Input> = self.inbox.borrow_mut().drain(..).collect();
        for input in inputs {
            match input {
                Input::Message { origin, data } => {
                    let _ = self.driver.message(&origin, true, &data);
                }
                Input::Open { socket, protocol } => self.driver.socket_open(socket, &protocol),
                Input::Data { socket, bytes } => self.driver.socket_message(socket, &bytes),
                Input::Closed { socket, code } => {
                    if self.socket.as_ref().is_some_and(|live| live.id == socket) {
                        self.socket = None;
                    }
                    self.driver.socket_closed(socket, code);
                }
                Input::Device(device) => self.driver.device_loaded(device),
                Input::Note(note) => self.driver.remote.say(note),
                Input::Page(PageEvent::Term(input)) => self.view.term.inputs.push(input),
                Input::Page(PageEvent::Viewport(viewport)) => self.view.viewport = viewport,
                Input::Page(PageEvent::Note(note)) => self.driver.remote.say(note),
            }
        }
        self.run_outputs();
        if !self.themed
            && let Some(boot) = self.driver.boot()
        {
            views::apply_theme(&mut self.view, boot);
            set_theme(&theme_colors(&self.view.looks.desktop));
            self.view.open(boot.view.as_deref().map_or(Tab::Board, Tab::named));
            self.view.ticket_key.clone_from(&boot.ticket_key);
            self.themed = true;
        }
        self.view.now_ms = page_now();
        if let Some(wait) = self.driver.wake_in() {
            ctx.request_repaint_after(Duration::from_millis(wait.max(16)));
        }
        if let Some(due) = self.view.term.due_ms() {
            let wait = (due - self.view.now_ms).max(16.0);
            ctx.request_repaint_after(Duration::from_millis(wait as u64));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        views::draw(ui, &mut self.driver, &mut self.client, &mut self.view);
        self.run_outputs();
        self.apply_overlay();
        if self.view.layout != self.saved_layout {
            save_layout(self.view.layout.name());
            self.saved_layout = self.view.layout;
        }
    }
}
