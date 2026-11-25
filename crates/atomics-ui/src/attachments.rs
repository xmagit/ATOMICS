//! One window-wide drop/confirmation hook, also usable by the simple-mode composer.
//! Only platform file input enters here. There is deliberately no serializable UI command for it.
use crate::Backend;
use atomics_core::attachments::{self, CHUNK_BYTES, Config, FileInfo, Reply, Request};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver},
};

#[derive(Clone)]
pub struct Prepared {
    pub info: FileInfo,
    pub bytes: Arc<[u8]>,
}
pub type Preparation = Receiver<Result<Vec<Prepared>, String>>;
pub type StageGuard = Box<dyn Send>;
pub type Staging = Receiver<Result<StageGuard, String>>;
/// Captures the platform file handles before requesting host limits.
pub type Selection = Box<dyn FnOnce(Config) -> Preparation + Send>;
pub type Picking = Receiver<Result<Option<Selection>, String>>;

/// Native platforms snapshot local files on a worker; a browser already owns the dropped bytes.
pub trait Source: Send + Sync {
    fn pick(&self) -> Option<Picking> {
        None
    }
    fn select(&self, files: Vec<egui::DroppedFileHandle>) -> Result<Selection, String>;
    fn exchange(&self, _limits: &Config) -> bool {
        false
    }
    fn stage(&self, _id: &str, _files: &[Prepared], _limits: &Config, _cancel: Arc<AtomicBool>) -> Staging {
        let (tx, rx) = mpsc::channel();
        let _ = tx.send(Err("No shared folder on this client".into()));
        rx
    }
}

pub struct UnavailableSource;
impl Source for UnavailableSource {
    fn select(&self, _: Vec<egui::DroppedFileHandle>) -> Result<Selection, String> {
        Err("No file input adapter on this platform".into())
    }
}

#[derive(Default)]
pub struct State {
    picking: Option<Picking>,
    files: Vec<Prepared>,
    selected: Option<Selection>,
    stage_guard: Option<StageGuard>,
    preparing: Option<Preparation>,
    staging: Option<Staging>,
    limits: Config,
    id: Option<String>,
    source: Option<Arc<dyn Source>>,
    waiting: bool,
    sending: bool,
    cancel: Arc<AtomicBool>,
    cancelling: bool,
    queued: bool,
    next_poll: f64,
    file: usize,
    offset: usize,
    note: Option<String>,
    visible: bool,
}

impl State {
    fn receive(&mut self, reply: Reply, backend: &dyn Backend) {
        if self.cancelling {
            if reply.queued && self.id.as_deref() == Some(&reply.id) {
                // The host has submitted the frame. Keep following this ticket until
                // the provider confirms delivery or the host reports a final error.
                self.queued = true;
                self.waiting = false;
                self.note = Some(reply.error.unwrap_or_else(|| "Waiting for chief delivery".into()));
            } else if let Some(error) = reply.error {
                self.fail(error);
            } else if self.id.is_none() && !reply.id.is_empty() && (!reply.complete || reply.queued) {
                self.id = Some(reply.id.clone());
                backend.attachment_request(Request::Cancel { id: reply.id });
            } else if reply.complete {
                let sent = self.queued || !reply.files.is_empty();
                self.fail(if sent { "Sent to the chief" } else { "Cancelled" }.into());
            }
            return;
        }
        if !self.waiting || (reply.complete && reply.error.is_none() && !self.sending) {
            return;
        }
        self.waiting = false;
        if let Some(error) = reply.error {
            self.fail(error);
            return;
        }
        if let Some(limits) = reply.limits {
            self.limits = limits;
            if let Some(selected) = self.selected.take() {
                self.preparing = Some(selected(self.limits.clone()));
                self.note = Some("Reading and checking files…".into());
            } else {
                self.fail("File input is unavailable".into());
            }
        } else if reply.complete {
            self.note = Some(
                if reply.retrying {
                    "Waiting to retry chief delivery"
                } else if reply.queued {
                    "Waiting for chief delivery"
                } else if reply.files.is_empty() && !self.queued {
                    "Cancelled"
                } else {
                    "Sent to the chief"
                }
                .into(),
            );
            self.stage_guard = None;
            self.files.clear();
            self.queued = reply.queued;
            self.sending = reply.queued;
            self.id = reply.queued.then_some(reply.id);
        } else if self.id.is_none() && self.sending {
            self.id = Some(reply.id.clone());
            if let Some(source) = &self.source
                && source.exchange(&self.limits)
            {
                self.staging = Some(source.stage(&reply.id, &self.files, &self.limits, self.cancel.clone()));
                self.note = Some("Copying to the shared folder…".into());
            } else {
                self.next(backend);
            }
        } else if self.sending {
            self.next(backend);
        }
    }
    fn next(&mut self, backend: &dyn Backend) {
        let Some(id) = self.id.clone() else {
            return;
        };
        while self.file < self.files.len() && self.offset == self.files[self.file].bytes.len() {
            self.file += 1;
            self.offset = 0;
        }
        if self.file == self.files.len() {
            backend.attachment_request(Request::Commit { id });
        } else {
            let file = &self.files[self.file];
            let end = (self.offset + CHUNK_BYTES).min(file.bytes.len());
            backend.attachment_request(Request::Chunk {
                id,
                file: self.file,
                offset: self.offset as u64,
                data: attachments::encode_chunk(&file.bytes[self.offset..end]),
            });
            self.offset = end;
        }
        self.waiting = true;
    }
    fn fail(&mut self, error: String) {
        self.cancel.store(true, Ordering::Release);
        self.cancelling = false;
        self.queued = false;
        self.note = Some(error);
        self.files.clear();
        self.selected = None;
        self.stage_guard = None;
        self.id = None;
        self.sending = false;
        self.waiting = false;
        self.preparing = None;
        self.staging = None;
    }
    fn send(&mut self, backend: &dyn Backend) {
        if !backend.can_attach() || self.files.is_empty() || self.sending {
            return;
        }
        self.sending = true;
        self.waiting = true;
        self.file = 0;
        self.offset = 0;
        self.note = Some("Uploading to the chief…".into());
        let view = backend.view_hints();
        let plan = view
            .plan_view
            .then(|| backend.plan_row(view.selected_plan))
            .flatten()
            .filter(|p| p.status == atomics_core::plan::PlanStatus::Open && !p.id.is_empty())
            .map(|p| p.id);
        if backend.attach_local(&self.files, plan.clone(), self.cancel.clone()) {
            return;
        }
        backend.attachment_request(Request::Begin {
            plan,
            files: self.files.iter().map(|f| f.info.clone()).collect(),
            exchange: self.source.as_ref().is_some_and(|s| s.exchange(&self.limits)),
        });
    }
}

/// Call once per frame, in either mode. egui drops are window-wide, independent of panel focus.
pub fn attach(ui: &mut egui::Ui, state: &mut State, backend: &dyn Backend) {
    let dropped = ui.input_mut(|i| std::mem::take(&mut i.raw.dropped_files));
    let picked = state.picking.as_ref().and_then(|rx| rx.try_recv().ok());
    if picked.is_some() {
        state.picking = None;
    }
    if (!dropped.is_empty() || picked.is_some()) && !state.sending && state.preparing.is_none() && !state.waiting {
        let source = backend.attachment_source();
        let selected = if !backend.can_attach() {
            Err("Only an authenticated controller may attach files".into())
        } else if let Some(selection) = picked {
            selection
        } else {
            source
                .as_ref()
                .ok_or_else(|| "File input is unavailable".to_owned())
                .and_then(|s| s.select(dropped))
                .map(Some)
        };
        match selected {
            Ok(Some(selected)) => {
                *state = State {
                    selected: Some(selected),
                    source,
                    visible: true,
                    waiting: true,
                    note: Some("Checking host limits…".into()),
                    ..Default::default()
                };
                backend.attachment_request(Request::Limits);
            }
            Ok(None) => {}
            Err(error) => {
                state.visible = true;
                state.fail(error);
            }
        }
    }
    while let Some(reply) = backend.attachment_reply() {
        if state.visible {
            state.receive(reply, backend);
        } else {
            state.waiting = false;
        }
    }
    if (state.sending || state.preparing.is_some() || state.waiting) && !backend.can_attach() {
        if let Some(id) = state.id.take() {
            backend.attachment_request(Request::Cancel { id });
        }
        state.fail("Disconnected or controller permission lost; drop the files again".into());
    }
    let now = ui.input(|i| i.time);
    if state.queued
        && !state.waiting
        && now >= state.next_poll
        && let Some(id) = &state.id
    {
        backend.attachment_request(Request::Status { id: id.clone() });
        state.next_poll = now + 0.25;
        state.waiting = true;
    }
    if let Some(prepare) = &state.preparing
        && let Ok(result) = prepare.try_recv()
    {
        state.preparing = None;
        match result {
            Ok(files) => {
                state.files = files;
                state.note = None;
            }
            Err(error) => state.fail(error),
        }
    }
    if let Some(staging) = &state.staging
        && let Ok(result) = staging.try_recv()
    {
        state.staging = None;
        match result {
            Ok(guard) => {
                state.stage_guard = Some(guard);
                if let Some(id) = &state.id {
                    backend.attachment_request(Request::Commit { id: id.clone() });
                    state.waiting = true;
                }
            }
            Err(error) => {
                if let Some(id) = state.id.take() {
                    backend.attachment_request(Request::Cancel { id });
                }
                state.fail(error);
            }
        }
    }
    if ui.input(|i| !i.raw.hovered_files.is_empty()) {
        let painter =
            ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::unique("attachment-drop")));
        let rect = ui.ctx().content_rect();
        painter.rect_filled(rect, 0.0, ui.visuals().window_fill().gamma_multiply(0.95));
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "Drop to send to the chief",
            egui::FontId::proportional(24.0),
            ui.visuals().text_color(),
        );
    }
    if state.visible {
        egui::Window::new("Files for the chief").collapsible(false).resizable(false).show(ui.ctx(), |ui| {
            for file in &state.files {
                ui.label(format!("{} · {} bytes", file.info.name, file.info.size));
            }
            if let Some(note) = &state.note {
                ui.label(note);
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !state.files.is_empty() && !state.sending && backend.can_attach(),
                        egui::Button::new("Send to chief"),
                    )
                    .clicked()
                {
                    state.send(backend);
                }
                if ui
                    .add_enabled(
                        !state.cancelling,
                        egui::Button::new(if state.sending || !state.files.is_empty() { "Cancel" } else { "Close" }),
                    )
                    .clicked()
                {
                    state.cancel.store(true, Ordering::Release);
                    state.staging = None;
                    state.stage_guard = None;
                    if state.sending {
                        state.cancelling = true;
                        state.waiting = true;
                        state.note = Some("Cancelling…".into());
                        if let Some(id) = &state.id {
                            backend.attachment_request(Request::Cancel { id: id.clone() });
                        }
                    } else {
                        *state = State { waiting: state.waiting, ..Default::default() };
                    }
                }
            });
        });
    }
    if state.queued || state.waiting || state.preparing.is_some() || state.staging.is_some() || state.picking.is_some()
    {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(30));
    }
}

/// A composer may offer the platform's file picker; its result follows the same confirmation hook.
pub fn button(ui: &mut egui::Ui, state: &mut State, backend: &dyn Backend, icon_size: Option<f32>) {
    let why = if !backend.can_attach() {
        "This window cannot attach files to the host"
    } else {
        "Attachments are already being picked or shown"
    };
    let label = if state.picking.is_some() { "Choosing files…" } else { "Attach files" };
    let button = match icon_size {
        Some(size) => egui::Button::new(
            crate::icons::text(crate::icons::ATTACH, 18.0).unwrap_or_else(|| egui::RichText::new("+")),
        )
        .min_size(egui::vec2(size, size))
        .corner_radius(7)
        .frame_when_inactive(false),
        None => crate::controls::bar_button(crate::icons::ATTACH, label).min_size(egui::vec2(0.0, 30.0)),
    };
    let response = ui
        .add_enabled(backend.can_attach() && !state.visible && state.picking.is_none(), button)
        .accessible_name(label)
        .on_hover_text(label)
        .on_disabled_hover_text(why);
    crate::flow::record(ui, "attach-files", response.rect);
    if response.clicked()
        && let Some(source) = backend.attachment_source()
    {
        state.picking = source.pick();
        if state.picking.is_none() {
            state.visible = true;
            state.fail("File picker unavailable. Drop files onto the window to attach them.".into());
        }
        ui.ctx().request_repaint();
    }
}
