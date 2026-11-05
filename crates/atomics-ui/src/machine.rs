//! The same host dashboard on the desktop, native remote client and browser.
use atomics_core::machine::Machine;
use atomics_core::preview::Preview;
use egui::{RichText, vec2};

use crate::{Backend, Client, Command};

pub fn button(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    if backend.machine_available() {
        let response = ui.selectable_label(client.machine_open, "Machine");
        crate::topbar::record_item(ui, "Machine", &response);
        if response.clicked() {
            client.machine_open = !client.machine_open;
            ui.close();
        }
    }
}

pub fn window(ctx: &egui::Context, backend: &dyn Backend, client: &mut Client) {
    if client.machine_open {
        let mut open = true;
        let p = client.palette.clone();
        egui::Window::new("Machine")
            .open(&mut open)
            .default_width((ctx.content_rect().width() - 32.0).min(760.0))
            .min_width(180.0)
            // A scroll area fills the window's remembered height rather than growing it, so the window opens
            // as tall as the screen allows and its contents scroll from there.
            .default_height((ctx.content_rect().height() - 100.0).clamp(160.0, 900.0))
            .max_width((ctx.content_rect().width() - 24.0).max(180.0))
            .frame(
                egui::Frame::new()
                    .fill(p.panel)
                    .inner_margin(12)
                    .stroke(egui::Stroke::new(1.0, p.border))
                    .corner_radius(6),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().max_height((ctx.content_rect().height() - 100.0).max(100.0)).show(
                    ui,
                    |ui| {
                        if let Some(snapshot) = backend.machine() {
                            contents(ui, &snapshot, backend, client);
                        } else {
                            ui.label("Waiting for machine data…");
                        }
                    },
                );
            });
        client.machine_open = open;
    }
    backend.watch_machine(client.machine_open);
}

pub fn contents(ui: &mut egui::Ui, snapshot: &Machine, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    if snapshot.truncated {
        ui.label(RichText::new("Some rows omitted to fit the snapshot limit").color(p.muted));
    }
    ui.label(RichText::new("Resources").color(p.foreground).strong());
    let load = snapshot.load.map_or_else(|| "—".into(), |v| format!("{:.1} / {:.1} / {:.1}", v[0], v[1], v[2]));
    ui.label(format!("{} cores · load 1 / 5 / 15 min: {load}", snapshot.cores));
    bar(
        ui,
        snapshot.cpu_busy.map(|v| v / 100.0),
        snapshot.cpu_busy.map_or_else(|| "CPU · collecting…".into(), |v| format!("CPU · {v:.0}% busy")),
        &p,
    );
    if let Some(memory) = &snapshot.memory {
        bar(
            ui,
            fraction(memory.total.saturating_sub(memory.available), memory.total),
            format!("Memory · {} available / {}", bytes(memory.available), bytes(memory.total)),
            &p,
        );
    } else {
        ui.label("Memory unavailable");
    }
    for disk in &snapshot.disks {
        ui.add(egui::Label::new(&disk.path).truncate()).on_hover_text(&disk.path);
        bar(
            ui,
            fraction(disk.total.saturating_sub(disk.free), disk.total),
            format!("Disk · {} free / {}", bytes(disk.free), bytes(disk.total)),
            &p,
        );
    }
    if let Some(resources) = backend.resources() {
        for (label, slots) in [("Test", resources.test), ("QA", resources.qa)] {
            ui.label(format!("{label} slots · {}/{} running · {} queued", slots.running, slots.slots, slots.waiting));
        }
    }
    ui.add_space(6.0);
    ui.separator();
    ui.label(RichText::new("Containers").color(p.foreground).strong());
    match &snapshot.containers {
        None => {
            ui.label(RichText::new("Docker not available").color(p.muted));
        }
        Some(containers) if containers.is_empty() => {
            ui.label("No running containers");
        }
        Some(containers) => {
            egui::ScrollArea::horizontal().id_salt("machine-containers").show(ui, |ui| {
                egui::Grid::new("containers-table").striped(true).min_col_width(65.0).show(ui, |ui| {
                    for heading in ["Name", "Image", "State / status", "CPU", "Memory", "Published ports"] {
                        ui.strong(heading);
                    }
                    ui.end_row();
                    for container in containers {
                        cell(ui, &container.name, 110.0);
                        cell(ui, &container.image, 140.0);
                        cell(ui, &format!("{} · {}", container.state, container.status), 170.0);
                        ui.label(container.cpu_percent.map_or_else(|| "—".into(), |v| format!("{v:.1}%")));
                        cell(ui, container.memory.as_deref().unwrap_or("—"), 125.0);
                        cell(ui, &container.ports, 200.0);
                        ui.end_row();
                    }
                });
            });
        }
    }
    ui.add_space(6.0);
    ui.separator();
    ui.label(RichText::new("Ports").color(p.foreground).strong());
    let sessions = backend.session_list();
    egui::ScrollArea::horizontal().id_salt("machine-ports").show(ui, |ui| {
        egui::Grid::new("ports-table").striped(true).min_col_width(65.0).show(ui, |ui| {
            for heading in ["Port", "Address", "Process / PID", "Agent", ""] {
                ui.strong(heading);
            }
            ui.end_row();
            for port in &snapshot.ports {
                ui.label(port.port.to_string());
                cell(ui, &port.address, 140.0);
                let owner = format!(
                    "{}{}",
                    port.process.as_deref().unwrap_or("—"),
                    port.pid.map_or_else(String::new, |pid| format!(" / {pid}"))
                );
                cell(ui, &owner, 160.0);
                let session = port
                    .session
                    .as_ref()
                    .map(|id| sessions.iter().find(|s| s.id == *id).map_or(id.as_str(), |s| s.title.as_str()));
                cell(ui, session.unwrap_or("—"), 120.0);
                if port.previewable && port.loopback_target() {
                    preview_button(ui, backend, client, None, Preview { port: port.port, ..Preview::default() });
                } else {
                    ui.label(RichText::new("Unavailable").color(p.muted));
                }
                ui.end_row();
            }
        });
    });
    if snapshot.ports.is_empty() {
        ui.label("No TCP listeners found");
    }
    ui.add_space(6.0);
    ui.separator();
    ui.label(RichText::new("Agent previews").color(p.foreground).strong());
    let mut count = 0;
    for session in sessions {
        if session.status == atomics_core::session::AgentStatus::Exited {
            continue;
        }
        for preview in backend.previews(Some(&session.id)) {
            count += 1;
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("{} · {}", session.title, preview.title()));
                preview_button(ui, backend, client, Some(session.id.clone()), preview);
            });
        }
    }
    if count == 0 {
        ui.label("No agent previews");
    }
}

fn preview_button(
    ui: &mut egui::Ui,
    backend: &dyn Backend,
    client: &mut Client,
    session: Option<String>,
    preview: Preview,
) {
    if ui
        .add_enabled(backend.can_preview(), egui::Button::new("Preview"))
        .on_disabled_hover_text("Preview needs a native client with tunnel support")
        .clicked()
    {
        client.send(Command::OpenPreview { session, preview: preview.clone() });
    }
    if backend.preview_active(preview.port) && ui.small_button("Stop").clicked() {
        client.send(Command::StopPreview { port: preview.port });
    }
}

fn cell(ui: &mut egui::Ui, text: &str, width: f32) {
    ui.add_sized(vec2(width, 18.0), egui::Label::new(text).truncate()).on_hover_text(text);
}

fn fraction(used: u64, total: u64) -> Option<f32> {
    (total > 0).then(|| (used as f64 / total as f64).clamp(0.0, 1.0) as f32)
}

fn bar(ui: &mut egui::Ui, value: Option<f32>, text: String, p: &crate::theme::Palette) {
    // The label crosses the fill and the empty track, so it takes the foreground colour, which reads on both.
    ui.add(
        egui::ProgressBar::new(value.unwrap_or(0.0))
            .fill(p.accent)
            .text(RichText::new(text).color(p.foreground))
            .desired_width(ui.available_width()),
    );
}

fn bytes(bytes: u64) -> String {
    let gib = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if gib >= 1.0 { format!("{gib:.1} GiB") } else { format!("{:.0} MiB", bytes as f64 / (1024.0 * 1024.0)) }
}
