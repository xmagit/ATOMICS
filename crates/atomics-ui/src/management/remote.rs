//! Pairing and trusted devices, drawn over host state.
use super::*;
use egui::{Color32, Rect, Vec2};

pub(super) fn draw(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, info: &ManagementInfo) {
    let remote = &info.remote;
    ui.add(egui::Label::new(RichText::new(&remote.status).color(client.palette.muted)).wrap());
    for problem in &remote.problems {
        ui.colored_label(client.palette.warning, problem);
    }
    let enabled = allowed(backend, &HostAction::PairDevice);
    if !enabled {
        ui.label("View only: pairing and revoking devices require the host grant.");
    }
    ui.add_enabled_ui(enabled, |ui| match &remote.pairing {
        None => {
            ui.label("Device pairing is not configured on this host.");
        }
        Some(PairingInfo::Idle { note }) => {
            if let Some(note) = note {
                ui.label(note);
            }
            if ui.button("Pair a device").clicked() {
                client.send(Command::Manage(HostAction::PairDevice));
            }
        }
        Some(PairingInfo::Open { link, seconds_left, busy }) => {
            ui.horizontal_wrapped(|ui| {
                if ui.button("Cancel pairing").clicked() {
                    client.send(Command::Manage(HostAction::CancelPairing));
                }
                ui.label(format!("Expires in {}:{:02}", seconds_left / 60, seconds_left % 60));
            });
            if let Some(link) = link {
                ui.label("Scan the code or open this link on your device:");
                qr_code(ui, link);
                ui.add(egui::Label::new(RichText::new(link).monospace().size(11.0)).wrap());
                if ui.button("Copy link").clicked() {
                    ui.ctx().copy_text(link.clone());
                }
            }
            if *busy {
                ui.label("A device is answering the link…");
            }
        }
        Some(PairingInfo::Confirm { name, code, seconds_left }) => {
            ui.label(format!("Pair “{name}”? Compare this code on both devices:"));
            ui.label(RichText::new(atomics_core::noise::show_code(code)).monospace().size(28.0));
            ui.label(format!("{seconds_left} seconds left"));
            for (label, controls) in [
                ("Codes match: may type and approve", Some(true)),
                ("Codes match: view only", Some(false)),
                ("Codes differ: refuse", None),
            ] {
                if ui.button(label).clicked() {
                    client.send(Command::Manage(HostAction::DecidePairing { code: code.clone(), controls }));
                }
            }
        }
    });
    ui.separator();
    ui.label(RichText::new("Paired devices").strong());
    if remote.devices.is_empty() {
        ui.label("No device is paired with this host.");
    }
    for device in &remote.devices {
        ui.label(format!(
            "{} · {} · {}",
            device.name,
            if device.controls { "may type and approve" } else { "view only" },
            if device.id.starts_with("paired-") {
                "paired client"
            } else if device.connected {
                "connected"
            } else {
                "offline"
            }
        ));
        if device.host {
            ui.label("Host requests allowed. Each action needs confirmation on the host desktop.");
        } else if device.controls {
            let grant = HostAction::GrantDeviceHost { id: device.id.clone() };
            if allowed(backend, &grant) {
                ui.label(
                    "Host access shares machine details and pairing links. Each Host command needs your approval.",
                );
                ui.label(egui::RichText::new(&device.id).monospace().small());
                if ui.push_id(&device.id, |ui| ui.button("Grant Host access on next connection")).inner.clicked() {
                    client.send(Command::Manage(grant));
                }
            }
        }
        let (paired, seen) = device.dates();
        ui.label(RichText::new(format!("Paired {paired} · Last seen {seen}")).small().color(client.palette.muted));
        ui.add_enabled_ui(enabled, |ui| {
            if client.management.revoke.as_deref() == Some(&device.id) {
                ui.colored_label(
                    client.palette.warning,
                    format!("Revoke {}? It must pair again to reach this host.", device.name),
                );
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Revoke").clicked() {
                        client.send(Command::Manage(HostAction::RevokeDevice { id: device.id.clone() }));
                        client.management.revoke = None;
                    }
                    if ui.button("Keep").clicked() {
                        client.management.revoke = None;
                    }
                });
            } else if ui.button("Revoke…").clicked() {
                client.management.revoke = Some(device.id.clone());
            }
        });
    }
}

fn qr_code(ui: &mut egui::Ui, text: &str) {
    let Ok(code) = qrcodegen::QrCode::encode_text(text, qrcodegen::QrCodeEcc::Medium) else {
        ui.label("Open the link on your device.");
        return;
    };
    let modules = code.size() + 8;
    let scale = (ui.available_width().min(260.0) / modules as f32).floor().max(1.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(scale * modules as f32), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, Color32::WHITE);
    for y in 0..code.size() {
        for x in 0..code.size() {
            if code.get_module(x, y) {
                let at = rect.min + Vec2::new((x + 4) as f32 * scale, (y + 4) as f32 * scale);
                painter.rect_filled(Rect::from_min_size(at, Vec2::splat(scale)), 0.0, Color32::BLACK);
            }
        }
    }
}
