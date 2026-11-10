//! The same host dashboard on the desktop, native remote client and browser.
use atomics_core::machine::Machine;
use atomics_core::preview::Preview;
use egui::{RichText, vec2};

use crate::{Backend, Client, Command};

/// Controls owned by a native client, such as the VM it connects to. They stay local to that window.
pub type Controls = std::sync::Arc<dyn Fn(&mut egui::Ui) + Send + Sync>;

pub fn button(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    if backend.machine_available() || client.machine_controls.is_some() {
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
        let snapshot = backend.machine();
        egui::Window::new(window_title(snapshot.as_ref(), client.machine_place.as_deref()))
            .id(egui::Id::unique("machine-window"))
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
                        if let Some(controls) = client.machine_controls.clone() {
                            controls(ui);
                            ui.separator();
                        }
                        if let Some(snapshot) = &snapshot {
                            contents(ui, snapshot, backend, client);
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
    crate::resources::disk_warning(ui, backend, client);
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
            let status =
                if snapshot.containers_timed_out { "Docker timed out · retrying…" } else { "Docker not available" };
            ui.label(RichText::new(status).color(p.muted));
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
            for heading in ["Port", "Address", "Process / PID", "Agent", "Preview"] {
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
                if let Some(reason) = preview_unavailable(port) {
                    ui.label(RichText::new("Unavailable").color(p.muted)).on_hover_text(reason);
                } else {
                    preview_button(ui, backend, client, None, Preview { port: port.port, ..Preview::default() });
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

fn window_title(snapshot: Option<&Machine>, place: Option<&str>) -> String {
    let host = snapshot.map(|s| s.host_name.as_str()).filter(|s| !s.is_empty()).unwrap_or("waiting for host name…");
    match place {
        Some(place) => format!("Machine · {host} · {place}"),
        None => format!("Machine · {host}"),
    }
}

fn preview_unavailable(port: &atomics_core::machine::Port) -> Option<&'static str> {
    // Process identity catches services even on nonstandard ports. Unknown application
    // processes stay eligible: development web servers may use arbitrary ports.
    let process = port.process.as_deref().unwrap_or("").to_ascii_lowercase();
    let process = process.rsplit('/').next().unwrap_or(&process);
    if [
        "sshd",
        "ssh",
        "dnsmasq",
        "systemd-resolve",
        "systemd-resolved",
        "named",
        "unbound",
        "avahi-daemon",
        "atomics",
        "postgres",
        "mysqld",
        "mariadbd",
        "redis-server",
        "mongod",
        "cupsd",
    ]
    .contains(&process)
        || process.contains("vnc")
    {
        return Some("This process provides a system or non-web service, not a web preview");
    }
    // Fallback for hidden owners and Docker's generic proxy processes.
    if (process.is_empty() || process == "docker-proxy")
        && matches!(port.port, 22 | 53 | 123 | 445 | 631 | 3306 | 5432 | 5900..=5999 | 6379 | 27017)
    {
        return Some("This port is normally used by a system or non-web service");
    }
    if !port.loopback_target() {
        return Some("This listener is bound to a specific interface; Preview connects through loopback");
    }
    if !port.previewable {
        return Some("The host has not made this listener available for Preview");
    }
    None
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
    if backend.preview_active(preview.port) && ui.button("Stop").clicked() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use atomics_core::machine::Port;

    #[test]
    fn title_names_the_host_and_reuses_the_clients_agent_location() {
        let snapshot = Machine { host_name: "test-host".into(), ..Default::default() };
        assert_eq!(window_title(Some(&snapshot), None), "Machine · test-host");
        assert_eq!(
            window_title(Some(&snapshot), Some("Agents in VM test-vm")),
            "Machine · test-host · Agents in VM test-vm"
        );
    }

    #[test]
    fn known_dev_servers_can_preview_on_conventional_service_ports() {
        let mut port = Port { address: "127.0.0.1".into(), previewable: true, ..Default::default() };
        for number in [22, 53, 5432, 5900, 5999] {
            port.port = number;
            for process in ["node", "python", "dotnet", "/usr/bin/node"] {
                port.process = Some(process.into());
                assert_eq!(preview_unavailable(&port), None, "{process} on {number}");
            }
            for process in [None, Some(""), Some("docker-proxy"), Some("/usr/bin/docker-proxy")] {
                port.process = process.map(str::to_string);
                assert!(preview_unavailable(&port).is_some(), "{process:?} on {number}");
            }
        }
        port.process = Some("node".into());
        port.previewable = false;
        assert!(preview_unavailable(&port).is_some());
    }

    #[test]
    fn system_services_have_no_preview_even_on_custom_ports() {
        let mut port = Port { address: "127.0.0.1".into(), port: 18080, previewable: true, ..Default::default() };
        for process in ["sshd", "systemd-resolve", "atomics", "Xtigervnc", "postgres", "/usr/sbin/dnsmasq"] {
            port.process = Some(process.into());
            assert!(preview_unavailable(&port).is_some(), "{process}");
        }
        for process in [None, Some("node"), Some("python"), Some("web"), Some("dotnet")] {
            port.process = process.map(str::to_string);
            assert_eq!(preview_unavailable(&port), None);
        }
        port.process = None;
        for number in [22, 53, 5900, 5901, 5432] {
            port.port = number;
            assert!(preview_unavailable(&port).is_some());
        }
        port.port = 8080;
        port.address = "192.0.2.1".into();
        assert!(preview_unavailable(&port).unwrap().contains("interface"));
        port.address = "127.0.0.1".into();
        port.previewable = false;
        assert!(preview_unavailable(&port).unwrap().contains("host"));
    }
}
