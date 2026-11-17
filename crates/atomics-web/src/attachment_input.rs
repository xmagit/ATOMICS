//! Browser-owned file handles. Synthetic events and postMessage cannot select a file.
use atomics_core::attachments::{self, Config, FileInfo};
use atomics_ui::attachments::{Picking, Preparation, Prepared, Source};
use std::{
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, mpsc},
};
use wasm_bindgen::{JsCast, closure::Closure};

#[derive(Debug)]
struct File {
    file: send_wrapper::SendWrapper<web_sys::File>,
    path: PathBuf,
}
impl egui::DroppedFile for File {
    fn path(&self) -> &Path {
        &self.path
    }
    fn bytes_async(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + '_>> {
        Box::pin(async move {
            let buffer = wasm_bindgen_futures::JsFuture::from(self.file.array_buffer())
                .await
                .map_err(|_| "Could not read file")?;
            Ok(js_sys::Uint8Array::new(&buffer).to_vec())
        })
    }
    fn web_file(&self) -> Option<&web_sys::File> {
        Some(&self.file)
    }
}
fn files(list: Option<web_sys::FileList>) -> Vec<egui::DroppedFileHandle> {
    list.map(|list| {
        (0..list.length())
            .filter_map(|i| list.get(i))
            .map(|file| {
                Arc::new(File { path: file.name().into(), file: send_wrapper::SendWrapper::new(file) })
                    as egui::DroppedFileHandle
            })
            .collect()
    })
    .unwrap_or_default()
}

#[derive(Debug)]
struct Folder;
impl egui::DroppedFile for Folder {
    fn path(&self) -> &Path {
        Path::new("Folder")
    }
    fn bytes_async(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>> + '_>> {
        Box::pin(async { Err("Folders cannot be attached".into()) })
    }
}

fn has_directory(data: &web_sys::DataTransfer) -> bool {
    (0..data.items().length()).filter_map(|i| data.items().get(i)).any(|item| {
        js_sys::Reflect::get(&item, &"webkitGetAsEntry".into())
            .ok()
            .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
            .and_then(|f| f.call0(&item).ok())
            .and_then(|entry| js_sys::Reflect::get(&entry, &"isDirectory".into()).ok())
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
    })
}

pub struct BrowserSource;
impl Source for BrowserSource {
    fn pick(&self) -> Option<Picking> {
        let document = web_sys::window()?.document()?;
        let input: web_sys::HtmlInputElement = document.create_element("input").ok()?.dyn_into().ok()?;
        input.set_type("file");
        input.set_multiple(true);
        let (tx, rx) = mpsc::channel();
        let picked = input.clone();
        let callback = Closure::once_into_js(move |event: web_sys::Event| {
            let selected = if event.is_trusted() && event.type_() == "change" { files(picked.files()) } else { vec![] };
            let _ = tx.send(selected);
        });
        let options = web_sys::AddEventListenerOptions::new();
        options.set_once(true);
        input
            .add_event_listener_with_callback_and_add_event_listener_options(
                "change",
                callback.unchecked_ref(),
                &options,
            )
            .ok()?;
        input
            .add_event_listener_with_callback_and_add_event_listener_options(
                "cancel",
                callback.unchecked_ref(),
                &options,
            )
            .ok()?;
        input.click();
        Some(rx)
    }
    fn prepare(&self, files: Vec<egui::DroppedFileHandle>, limits: Config) -> Preparation {
        let (tx, rx) = mpsc::channel();
        wasm_bindgen_futures::spawn_local(async move {
            let result = async {
                if files.is_empty() || files.len() > limits.max_files {
                    return Err("Too many files in this drop".into());
                }
                let mut total = 0u64;
                for file in &files {
                    let size = file.web_file().ok_or("Folders cannot be attached")?.size() as u64;
                    total = total.checked_add(size).ok_or("Drop is too large")?;
                    if size > limits.max_file_bytes || total > limits.max_drop_bytes {
                        return Err("Drop exceeds the attachment byte limit".into());
                    }
                }
                let mut prepared = Vec::new();
                for file in files {
                    let bytes: Arc<[u8]> = file.bytes_async().await?.into();
                    let info = FileInfo {
                        name: attachments::safe_name(&file.path().to_string_lossy()),
                        size: bytes.len() as u64,
                        sha256: attachments::hash(&bytes),
                    };
                    prepared.push(Prepared { info, bytes });
                }
                attachments::validate(&prepared.iter().map(|f| f.info.clone()).collect::<Vec<_>>(), &limits)?;
                Ok(prepared)
            }
            .await;
            let _ = tx.send(result);
        });
        rx
    }
}

pub struct Input {
    pending: Rc<RefCell<Vec<egui::DroppedFileHandle>>>,
    callback: Closure<dyn FnMut(web_sys::DragEvent)>,
    hover_callback: Closure<dyn FnMut(web_sys::DragEvent)>,
    hovered: Rc<Cell<bool>>,
}
impl Input {
    pub fn new(ctx: egui::Context) -> Self {
        let pending: Rc<RefCell<Vec<egui::DroppedFileHandle>>> = Rc::default();
        let output = pending.clone();
        let hovered = Rc::new(Cell::new(false));
        let hover = hovered.clone();
        let repaint = ctx.clone();
        let hover_callback = Closure::new(move |event: web_sys::DragEvent| {
            if event.is_trusted() {
                event.prevent_default();
                hover.set(event.type_() == "dragover");
                repaint.request_repaint();
            }
        });
        let hover = hovered.clone();
        let callback = Closure::new(move |event: web_sys::DragEvent| {
            event.prevent_default();
            event.stop_immediate_propagation();
            if event.is_trusted()
                && let Some(data) = event.data_transfer()
            {
                hover.set(false);
                *output.borrow_mut() = if has_directory(&data) { vec![Arc::new(Folder)] } else { files(data.files()) };
                ctx.request_repaint();
            }
        });
        if let Some(window) = web_sys::window() {
            let _ = window.add_event_listener_with_callback_and_bool("drop", callback.as_ref().unchecked_ref(), true);
            for event in ["dragover", "dragleave"] {
                let _ = window.add_event_listener_with_callback_and_bool(
                    event,
                    hover_callback.as_ref().unchecked_ref(),
                    true,
                );
            }
        }
        Self { pending, callback, hover_callback, hovered }
    }
    pub fn take(&self) -> Vec<egui::DroppedFileHandle> {
        std::mem::take(&mut self.pending.borrow_mut())
    }
    pub fn hovered(&self) -> bool {
        self.hovered.get()
    }
}
impl Drop for Input {
    fn drop(&mut self) {
        if let Some(window) = web_sys::window() {
            for event in ["dragover", "dragleave"] {
                let _ = window.remove_event_listener_with_callback_and_bool(
                    event,
                    self.hover_callback.as_ref().unchecked_ref(),
                    true,
                );
            }
            let _ = window.remove_event_listener_with_callback_and_bool(
                "drop",
                self.callback.as_ref().unchecked_ref(),
                true,
            );
        }
    }
}
