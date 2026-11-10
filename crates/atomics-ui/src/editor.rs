//! Host editor PTYs, drawn through the same terminal surface as agent sessions.
use crate::keys::shortcut_label;
use crate::terminal::Metrics;
use crate::terminal_surface::TerminalSurface;
use crate::{Backend, Client, Command, PanelMode};
use egui::{Rect, RichText, Stroke, vec2};
use std::path::Path;

pub fn draw(
    ui: &mut egui::Ui,
    rect: Rect,
    root: &Path,
    backend: &dyn Backend,
    client: &mut Client,
    terminals: &mut dyn TerminalSurface,
) {
    let p = client.palette.clone();
    let editor = backend.editors().into_iter().find(|e| Path::new(&e.root) == root);
    ui.painter().rect_filled(rect, 0.0, p.terminal);
    let touch = client.touch;
    let header = rect.with_max_y(rect.top() + touch.map_or(30.0, |t| t.target + 8.0));
    let body = rect.with_min_y(header.bottom());
    ui.painter().rect_filled(header, 0.0, p.raised);
    ui.painter().line_segment([header.left_bottom(), header.right_bottom()], Stroke::new(1.0, p.border));
    let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("editor-header")
            .max_rect(header.shrink2(vec2(10.0, 3.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    crate::sidebar::section(&mut child, &p, "EDITOR");
    child.label(RichText::new(&name).color(p.muted).size(12.0));
    let editor_running = editor.as_ref().is_some_and(|e| e.running);
    let mut action = None;
    child.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if crate::touch::button(ui, touch, "×").on_hover_text("Close the panel").clicked() {
            action = Some("close");
        }
        if crate::touch::button(ui, touch, "Back to diff")
            .on_hover_text(format!(
                "Back to the diff ({})",
                shortcut_label(ui.ctx(), &backend.ui_config().keys.toggle_editor)
            ))
            .clicked()
        {
            action = Some("diff");
        }
        if editor_running
            && editor.as_ref().is_some_and(|e| e.inlay_hints)
            && backend.available(&Command::EditorHints(root.to_path_buf()))
            && crate::touch::button(ui, touch, "Inlay hints")
                .on_hover_text("Toggle the language server's inlay hints")
                .clicked()
        {
            action = Some("hints");
        }
    });
    match action {
        Some("close") => client.send(Command::CloseDiff),
        Some("diff") => client.send(Command::SetPanel(PanelMode::Diff)),
        Some("hints") => client.send(Command::EditorHints(root.to_path_buf())),
        _ => {}
    }

    let metrics = Metrics::new(ui, &p, backend.ui_config().font.size);
    let id = ui.scope_id().with(("editor", root));
    client.editor_id = Some(id);
    match editor {
        Some(editor) if editor.running => {
            if std::mem::take(&mut client.focus_editor) {
                ui.memory_mut(|m| m.request_focus(id));
            }
            terminals.show(ui, usize::MAX, &editor.terminal, body, id, &metrics, &p);
        }
        editor => {
            let mut child = ui.new_child(egui::UiBuilder::new().id_salt("editor-empty").max_rect(body.shrink(20.0)));
            match editor.and_then(|e| e.error) {
                Some(error) => {
                    child.label(RichText::new("The editor could not start.").color(p.muted));
                    child.label(RichText::new(error).color(p.warning));
                }
                None => {
                    child.label(RichText::new("The editor is not running.").color(p.muted));
                }
            }
            if backend.available(&Command::Edit { root: root.to_path_buf(), path: None, line: None })
                && child.button("Open editor").clicked()
            {
                client.send(Command::Edit { root: root.to_path_buf(), path: None, line: None });
            }
        }
    }
}
