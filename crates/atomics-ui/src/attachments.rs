//! One window-wide drop/confirmation hook, also usable by the simple-mode composer.
//! Only platform file input enters here. There is deliberately no serializable UI command for it.
use crate::Backend;
use atomics_core::attachments::{self, CHUNK_BYTES, Config, FileInfo, Reply, Request};
use std::sync::{
    Arc,
    mpsc::{self, Receiver},
};

#[derive(Clone)]
pub struct Prepared {
    pub info: FileInfo,
    pub bytes: Arc<[u8]>,
}
pub type Preparation = Receiver<Result<Vec<Prepared>, String>>;
pub type Staging = Receiver<Result<(), String>>;
pub type Picking = Receiver<Vec<egui::DroppedFileHandle>>;

/// Native platforms snapshot local files on a worker; a browser already owns the dropped bytes.
pub trait Source: Send + Sync {
    fn pick(&self) -> Option<Picking> {
        None
    }
    fn prepare(&self, files: Vec<egui::DroppedFileHandle>, limits: Config) -> Preparation;
    fn exchange(&self, _limits: &Config) -> bool {
        false
    }
    fn stage(&self, _id: &str, _files: &[Prepared], _limits: &Config) -> Staging {
        let (tx, rx) = mpsc::channel();
        let _ = tx.send(Err("No shared folder on this client".into()));
        rx
    }
}

pub struct UnavailableSource;
impl Source for UnavailableSource {
    fn prepare(&self, files: Vec<egui::DroppedFileHandle>, limits: Config) -> Preparation {
        let (tx, rx) = mpsc::channel();
        let _ = (files, limits);
        let _ = tx.send(Err("No file input adapter on this platform".into()));
        rx
    }
}

#[derive(Default)]
pub struct State {
    picking: Option<Picking>,
    files: Vec<Prepared>,
    dropped: Vec<egui::DroppedFileHandle>,
    preparing: Option<Preparation>,
    staging: Option<Staging>,
    limits: Config,
    id: Option<String>,
    source: Option<Arc<dyn Source>>,
    waiting: bool,
    sending: bool,
    file: usize,
    offset: usize,
    note: Option<String>,
    visible: bool,
}

impl State {
    fn receive(&mut self, reply: Reply, backend: &dyn Backend) {
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
            self.source = backend.attachment_source();
            if let Some(source) = &self.source {
                self.preparing = Some(source.prepare(std::mem::take(&mut self.dropped), self.limits.clone()));
                self.note = Some("Reading and checking files…".into());
            } else {
                self.fail("File input is unavailable".into());
            }
        } else if reply.complete {
            self.note = Some(if reply.files.is_empty() { "Cancelled" } else { "Sent to the chief" }.into());
            self.files.clear();
            self.sending = false;
            self.id = None;
        } else if self.id.is_none() && self.sending {
            self.id = Some(reply.id.clone());
            if let Some(source) = &self.source
                && source.exchange(&self.limits)
            {
                self.staging = Some(source.stage(&reply.id, &self.files, &self.limits));
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
        self.note = Some(error);
        self.files.clear();
        self.dropped.clear();
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
        backend.attachment_request(Request::Begin {
            files: self.files.iter().map(|f| f.info.clone()).collect(),
            exchange: self.source.as_ref().is_some_and(|s| s.exchange(&self.limits)),
        });
    }
}

/// Call once per frame, in either mode. egui drops are window-wide, independent of panel focus.
pub fn attach(ui: &mut egui::Ui, state: &mut State, backend: &dyn Backend) {
    let mut dropped = ui.input_mut(|i| std::mem::take(&mut i.raw.dropped_files));
    if let Some(picking) = &state.picking
        && let Ok(files) = picking.try_recv()
    {
        dropped.extend(files);
        state.picking = None;
    }
    if !dropped.is_empty() {
        if !backend.can_attach() {
            state.visible = true;
            state.fail("Only an authenticated controller may attach files".into());
        } else if !state.sending && state.preparing.is_none() && !state.waiting {
            *state = State {
                dropped,
                visible: true,
                waiting: true,
                note: Some("Checking host limits…".into()),
                ..Default::default()
            };
            backend.attachment_request(Request::Limits);
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
            Ok(()) => {
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
                if ui.add_enabled(!state.sending, egui::Button::new("Cancel")).clicked() {
                    if let Some(id) = state.id.take() {
                        backend.attachment_request(Request::Cancel { id });
                    }
                    *state = State { waiting: state.waiting, ..Default::default() };
                }
            });
        });
    }
    if state.waiting || state.preparing.is_some() || state.staging.is_some() || state.picking.is_some() {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(30));
    }
}

/// A composer may offer the platform's file picker; its result follows the same confirmation hook.
pub fn button(ui: &mut egui::Ui, state: &mut State, backend: &dyn Backend) {
    if ui
        .add_enabled(
            backend.can_attach() && !state.visible && state.picking.is_none(),
            egui::Button::new("Attach files"),
        )
        .clicked()
        && let Some(source) = backend.attachment_source()
    {
        state.picking = source.pick();
    }
}
