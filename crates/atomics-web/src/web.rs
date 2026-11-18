//! The browser glue: eframe's web runner, the window's message events, the broker's WebSocket and the
//! device store, each turned into the driver's inputs, and the driver's outputs carried out. Nothing is
//! decided here; the driver decides, and it is tested natively.
//!
//! The parent origin comes from the hosting page's config script (`config.js` beside the bundle sets
//! `window.ATOMICS_PARENT_ORIGIN`). A message from any other origin, or from a window other than the one
//! that framed this page, is dropped before its data is read.
//!
//! Terminal input is the page's `<textarea>` overlay and key bar (`web/terminal.js`): what it reports is
//! read by [`crate::page`] and handed to the terminal pane, and after each frame the overlay is placed,
//! clipped and set as the pane says ([`Overlay`]).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Duration;

use atomics_core::noise::{KEY_BYTES, PublicKey, Secret};
use atomics_core::tunnel;
use atomics_ui::theme::Palette;
use atomics_ui::{Client, PanelMode};
use js_sys::{Array, ArrayBuffer, JSON, Object, Reflect, Uint8Array};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{BinaryType, CloseEvent, HtmlCanvasElement, MessageEvent, WebSocket, Window};
use zeroize::Zeroizing;

use crate::boot::{self, ParentOrigin};
use crate::desk;
use crate::device::{self, Device};
use crate::driver::{DeviceReadError, Driver, Output};
use crate::keep::{Answer, Ask, Failure, Passkeys, Sealed};
use crate::layout;
use crate::page::{self, PageEvent};
use crate::storage::Storage;
use crate::term_pane::{self, Overlay};
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
    #[wasm_bindgen(js_name = requestPersistence)]
    async fn request_persistence() -> JsValue;
    #[wasm_bindgen(js_name = storageFacts)]
    async fn storage_facts(host_id: &str) -> JsValue;
}

#[wasm_bindgen(module = "/web/passkey.js")]
extern "C" {
    #[wasm_bindgen(js_name = passkeyFacts)]
    async fn passkey_facts() -> JsValue;
    #[wasm_bindgen(js_name = installPasskey)]
    fn install_passkey(callback: &Closure<dyn FnMut(JsValue)>);
    #[wasm_bindgen(js_name = preparePasskey)]
    fn prepare_passkey(op: &str, secret: &[u8]);
    #[wasm_bindgen(js_name = clearPasskey)]
    fn clear_passkey();
    #[wasm_bindgen(js_name = placePasskey)]
    fn place_passkey(x: f32, y: f32, width: f32, height: f32, label: &str);
    #[wasm_bindgen(js_name = hidePasskey)]
    fn hide_passkey();
}

#[wasm_bindgen(module = "/web/terminal.js")]
extern "C" {
    fn install(keys: &str, command_chords: bool, callback: &Closure<dyn FnMut(JsValue)>);
    fn place(x: f32, y: f32, width: f32, height: f32);
    #[wasm_bindgen(js_name = setClip)]
    fn set_clip(clip: &str);
    #[wasm_bindgen(js_name = placeStrip)]
    fn place_strip(x: f32, y: f32, width: f32, height: f32);
    #[wasm_bindgen(js_name = setTextBoxes)]
    fn set_text_boxes(boxes: &str);
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
    fn set_copy(text: Option<String>, x: f32, y: f32, below: f32);
    #[wasm_bindgen(js_name = setTheme)]
    fn set_theme(colors: &str);
    #[wasm_bindgen(js_name = loadView)]
    fn load_view() -> String;
    #[wasm_bindgen(js_name = saveView)]
    fn save_view(name: &str);
    #[wasm_bindgen(js_name = loadMode)]
    fn load_mode() -> String;
    #[wasm_bindgen(js_name = saveMode)]
    fn save_mode(name: &str);
    #[wasm_bindgen(js_name = setDrawer)]
    fn set_drawer(open: bool);
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

/// What the device store knows about its storage for `host`, also written to the console for a remote
/// inspector. Facts only, never key material.
async fn storage_report(host: &str) -> Storage {
    let storage = Storage::read(&value(&storage_facts(host).await));
    web_sys::console::info_1(&JsValue::from_str(&storage.line()));
    storage
}

/// A JavaScript value as JSON, or null.
fn value(js: &JsValue) -> serde_json::Value {
    JSON::stringify(js)
        .ok()
        .and_then(|text| serde_json::from_str(&String::from(text)).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// What `web/passkey.js` said came of a tap. An unlocked key is copied out of its array and the array zeroed; it
/// is never turned into text.
fn passkey_answer(js: &JsValue) -> Option<Answer> {
    let kind = Reflect::get(js, &JsValue::from_str("kind")).ok()?.as_string()?;
    let failed = |step: &str, detail: &str| {
        Answer::Failed(Failure { step: step.into(), name: "DataError".into(), detail: detail.into() })
    };
    if kind == "unlocked" {
        let secret = Reflect::get(js, &JsValue::from_str("secret")).ok()?.dyn_into::<Uint8Array>().ok()?;
        let mut bytes = Zeroizing::new([0u8; KEY_BYTES]);
        let whole = secret.length() as usize == KEY_BYTES;
        if whole {
            secret.copy_to(&mut bytes[..]);
        }
        secret.fill(0, 0, secret.length());
        return Some(if whole {
            Answer::Unlocked(Secret::from_bytes(&bytes))
        } else {
            failed("open", "not a device key")
        });
    }
    let data = value(js);
    Some(match kind.as_str() {
        "sealed" => {
            Sealed::read(&data["record"]).map_or_else(|| failed("seal", "the record was malformed"), Answer::Sealed)
        }
        "enrolling" => Answer::Enrolling,
        "enrolled" => Answer::Enrolled(data["credential"].as_str().map(str::to_string)),
        "no-prf" => Answer::NoPrf,
        "failed" => Answer::Failed(Failure::read(&data)),
        _ => return None,
    })
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
    Device(Result<Option<Device>, DeviceReadError>),
    Saved(Result<(), String>),
    Storage(Storage),
    Passkeys(Passkeys),
    Answer(Answer),
    Resumed { hidden_ms: u64 },
    Note(String),
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
    file_input: crate::attachment_input::Input,
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
    _visibility: Closure<dyn FnMut()>,
    _passkey: Closure<dyn FnMut(JsValue)>,
    /// Where the passkey button was last laid, and what it said.
    passkey_placed: Option<(egui::Rect, String)>,
    /// What the overlay was last set to.
    applied: Overlay,
    /// Whether the page was last told the drawer is open, which gives it a step in the tab's history.
    drawer: bool,
    /// The text boxes the page was last told of.
    applied_boxes: Vec<egui::Rect>,
    /// The view the switcher chose that this browser keeps, as last saved.
    saved_view: PanelMode,
    /// This browser's own choice of simple or full layout, as last saved; never the host's config.
    saved_mode: Option<atomics_core::config::UiMode>,
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

/// The ATOMICS icon, for the top bar's brand.
fn load_icon(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let decoder = png::Decoder::new(std::io::Cursor::new(include_bytes!("../../../assets/icon.png")));
    let mut reader = decoder.read_info().ok()?;
    let mut rgba = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut rgba).ok()?;
    rgba.truncate(info.buffer_size());
    let size = [info.width as usize, info.height as usize];
    let image = egui::ColorImage::from_rgba_unmultiplied(size, &rgba);
    Some(ctx.load_texture("atomics-icon", image, egui::TextureOptions::LINEAR))
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
        driver.remote.set_attachment_source(std::sync::Arc::new(crate::attachment_input::BrowserSource));
        let file_input = crate::attachment_input::Input::new(ctx.clone());
        let mut client = Client::default();
        client.palette.apply(&ctx, &BundledFonts);
        client.icon = load_icon(&ctx);
        let inbox: Inbox = Rc::default();
        // Best effort, and silent: a refusal is nothing a person can act on.
        wasm_bindgen_futures::spawn_local(async {
            let _ = request_persistence().await;
        });
        {
            let (inbox, ctx) = (inbox.clone(), ctx.clone());
            wasm_bindgen_futures::spawn_local(async move {
                let passkeys = Passkeys::read(&value(&passkey_facts().await));
                push(&inbox, &ctx, Input::Passkeys(passkeys));
            });
        }
        let passkey = {
            let (inbox, ctx) = (inbox.clone(), ctx.clone());
            Closure::<dyn FnMut(JsValue)>::new(move |answer: JsValue| {
                if let Some(answer) = passkey_answer(&answer) {
                    push(&inbox, &ctx, Input::Answer(answer));
                }
            })
        };
        install_passkey(&passkey);
        let visibility = {
            let (inbox, ctx) = (inbox.clone(), ctx.clone());
            let document = window.document().expect("a document");
            let visible = document.clone();
            let mut hidden_at = visible.hidden().then(|| js_sys::Date::now() as u64);
            let listener = Closure::<dyn FnMut()>::new(move || {
                if visible.hidden() {
                    hidden_at.get_or_insert_with(|| js_sys::Date::now() as u64);
                } else {
                    let hidden_ms = hidden_at.take().map_or(0, |at| (js_sys::Date::now() as u64).saturating_sub(at));
                    push(&inbox, &ctx, Input::Resumed { hidden_ms });
                }
            });
            let _ = document.add_event_listener_with_callback("visibilitychange", listener.as_ref().unchecked_ref());
            listener
        };
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
        install(&layout::keybar_json().to_string(), atomics_ui::window_keys::command_chords(ctx.os()), &page);
        set_theme(&theme_colors(&client.palette));
        let mut view = View::default();
        // The view the switcher last showed in this browser.
        if let Some(panel) = desk::named_view(&load_view()) {
            view.desk.panel = panel;
        }
        let saved_view = view.desk.panel;
        // Simple or full, as this browser last chose; nothing kept follows the host's `[ui] mode`.
        client.mode = atomics_ui::simple::named_mode(&load_mode());
        let saved_mode = client.mode;
        Self {
            file_input,
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
            _visibility: visibility,
            _passkey: passkey,
            passkey_placed: None,
            applied: Overlay::default(),
            drawer: false,
            applied_boxes: Vec::new(),
            saved_view,
            saved_mode,
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
        if let Some(rect) = now.rect.filter(|_| now.rect != last.rect || now.covered != last.covered) {
            set_clip(&term_pane::clip_path(rect, &now.covered, zoom));
        }
        if now.strip != last.strip {
            let r = now.strip.unwrap_or(egui::Rect::ZERO);
            place_strip(r.left() * zoom, r.top() * zoom, r.width() * zoom, r.height() * zoom);
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
                Some((text, at, below)) => set_copy(Some(text.clone()), at.x * zoom, at.y * zoom, below * zoom),
                None => set_copy(None, 0.0, 0.0, 0.0),
            }
        }
        self.applied = now;
        if self.view.text_boxes != self.applied_boxes {
            set_text_boxes(&views::text_boxes_json(&self.view.text_boxes, zoom));
            self.applied_boxes.clone_from(&self.view.text_boxes);
        }
    }

    fn post(&self, kind: &str) {
        self.post_value(&boot::say(kind));
    }

    /// Post to the parent, at the parent origin exactly.
    fn post_value(&self, message: &serde_json::Value) {
        if let Some((framer, origin)) = &self.parent {
            let _ = framer.post_message(&json(message), origin.as_str());
        }
    }

    /// Lay the passkey button where this frame's screen put its place ([`crate::views::keep::button_id`]), or take it
    /// away, telling the page only what changed.
    fn apply_passkey(&mut self) {
        let id = crate::views::keep::button_id();
        let placed = self.ctx.data_mut(|d| d.remove_temp::<Option<(egui::Rect, String)>>(id)).flatten();
        if placed == self.passkey_placed {
            return;
        }
        match &placed {
            Some((r, label)) => {
                let zoom = self.ctx.zoom_factor();
                place_passkey(r.left() * zoom, r.top() * zoom, r.width() * zoom, r.height() * zoom, label);
            }
            None => hide_passkey(),
        }
        self.passkey_placed = placed;
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
            // Only a native client dials its host directly.
            Output::DialDirect { socket } => push(&self.inbox, &self.ctx, Input::Closed { socket, code: 1006 }),
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
                    let device = match load_device(&host).await {
                        Ok(stored) => stored_device(&host, &stored),
                        Err(_) => Err(DeviceReadError),
                    };
                    // Before the answer, so a pairing it leads to names what storage held.
                    push(&inbox, &ctx, Input::Storage(storage_report(&host).await));
                    push(&inbox, &ctx, Input::Device(device));
                });
            }
            Output::SaveDevice(device) => {
                let (inbox, ctx) = (self.inbox.clone(), self.ctx.clone());
                wasm_bindgen_futures::spawn_local(async move {
                    let key = device.host_key.text();
                    let saved = save_device(&device.host_id, &key, device.secret.as_bytes()).await.is_ok();
                    if saved {
                        let _ = request_persistence().await;
                    }
                    let storage = storage_report(&device.host_id).await;
                    if !saved {
                        let note = format!(
                            "This browser could not save its pairing ({}). Reopening this page may require pairing again.",
                            storage.save
                        );
                        push(&inbox, &ctx, Input::Note(note));
                    }
                    let result = if saved { Ok(()) } else { Err(storage.save.clone()) };
                    push(&inbox, &ctx, Input::Storage(storage));
                    push(&inbox, &ctx, Input::Saved(result));
                });
            }
            Output::Paired { kept } => self.post_value(&boot::say_paired(kept)),
            Output::Keep(sealed) => self.post_value(&boot::say_keep(&sealed)),
            Output::Forget(host) => self.post_value(&boot::say_forget(&host)),
            Output::Passkey(Ask::Keep { device, credential, made, create_in_frame }) => {
                let op = serde_json::json!({
                    "kind": "keep", "hostId": device.host_id, "hostKey": device.host_key.text(),
                    "credential": credential, "made": made, "inFrame": create_in_frame,
                });
                prepare_passkey(&op.to_string(), device.secret.as_bytes());
            }
            Output::Passkey(Ask::Unlock(sealed)) => {
                prepare_passkey(&serde_json::json!({ "kind": "unlock", "record": sealed.to_json() }).to_string(), &[]);
            }
            Output::Passkey(Ask::Done) => {
                clear_passkey();
                self.passkey_placed = None;
            }
            Output::ForgetDevice(host) => {
                let (inbox, ctx) = (self.inbox.clone(), self.ctx.clone());
                wasm_bindgen_futures::spawn_local(async move {
                    let _ = forget_device(&host).await;
                    push(&inbox, &ctx, Input::Storage(storage_report(&host).await));
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
fn stored_device(host_id: &str, stored: &JsValue) -> Result<Option<Device>, DeviceReadError> {
    if stored.is_null() {
        return Ok(None);
    }
    let host_key = Reflect::get(stored, &JsValue::from_str("hostKey")).ok().and_then(|v| v.as_string());
    let secret = Reflect::get(stored, &JsValue::from_str("secret"))
        .ok()
        .and_then(|v| v.dyn_into::<Uint8Array>().ok())
        .ok_or(DeviceReadError)?;
    let mut bytes = Zeroizing::new([0u8; KEY_BYTES]);
    let read = secret.length() as usize == KEY_BYTES;
    if read {
        secret.copy_to(&mut bytes[..]);
    }
    secret.fill(0, 0, secret.length());
    let host_key = host_key.as_deref().and_then(PublicKey::parse).ok_or(DeviceReadError)?;
    if !read {
        return Err(DeviceReadError);
    }
    Ok(Some(Device { host_id: host_id.to_string(), host_key, secret: Secret::from_bytes(&bytes) }))
}

impl eframe::App for App {
    /// What a soft keyboard or a notch covers takes no window, menu or dialog, wherever they are drawn.
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        raw_input.dropped_files = self.file_input.take();
        raw_input.hovered_files = if self.file_input.hovered() { vec![egui::HoveredFile::default()] } else { vec![] };
        if let Some(screen) = raw_input.screen_rect {
            raw_input.safe_area_insets = Some(views::safe_area(&self.view, screen, ctx.zoom_factor()));
        }
    }

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
                Input::Saved(saved) => self.driver.device_saved(saved),
                Input::Storage(storage) => self.driver.storage_noted(storage),
                Input::Passkeys(passkeys) => self.driver.passkeys_noted(passkeys),
                Input::Answer(answer) => self.driver.passkey_answered(answer),
                Input::Resumed { hidden_ms } => self.driver.resumed(hidden_ms),
                Input::Note(note) => self.driver.remote.say(note),
                Input::Page(PageEvent::Term(input)) => self.view.term.inputs.push(input),
                Input::Page(PageEvent::Viewport(viewport)) => self.view.viewport = viewport,
                Input::Page(PageEvent::Note(note)) => self.driver.remote.say(note),
                Input::Page(PageEvent::Back) => self.view.back = true,
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
        self.apply_passkey();
        if self.client.drawer != self.drawer {
            self.drawer = self.client.drawer;
            set_drawer(self.drawer);
        }
        if self.view.desk.panel != self.saved_view {
            save_view(desk::view_name(self.view.desk.panel));
            self.saved_view = self.view.desk.panel;
        }
        if self.client.mode != self.saved_mode {
            save_mode(atomics_ui::simple::mode_name(self.client.mode));
            self.saved_mode = self.client.mode;
        }
    }
}
