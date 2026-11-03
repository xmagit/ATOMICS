//! The banner over a session's tile while the session waits on a permission request.

use atomics_core::approvals::Decision;
use egui::{Rect, RichText};

use crate::touch;
use crate::{Backend, Client, Command};

/// Draws the first permission request the session at `index` of [`Backend::session_list`] waits on at the
/// top of its tile's `body`, and returns the part of `body` below it; with no request waiting, `body` as it
/// is. Allow, Allow always (never for Azure REST) and Deny record a [`Command::Decide`] once per request;
/// the buttons stay disabled until the host has decided, and are not drawn where the backend cannot decide.
pub fn banner(ui: &mut egui::Ui, body: Rect, index: usize, backend: &dyn Backend, client: &mut Client) -> Rect {
    let Some(pending) = backend.session_approval(index) else { return body };
    let p = client.palette.clone();
    let touch = client.touch;
    let request = &pending.request;
    let deciding = client.approval_decisions.contains(&(pending.session.clone(), pending.id.clone()));
    let height = (body.height() - 220.0).clamp(110.0, 210.0).min(body.height());
    let banner = body.with_max_y(body.top() + height);
    ui.painter().rect_filled(banner, 0.0, p.panel);
    // On a phone the banner spans the screen, and its content keeps to the screen's gutter.
    let inner = touch.map_or(banner.shrink(6.0), |touch| banner.shrink2(egui::vec2(touch.gutter, 8.0)));
    let mut child = ui.new_child(egui::UiBuilder::new().id_salt(("approval", &pending.session)).max_rect(inner));
    child.set_clip_rect(banner);
    child.label(RichText::new("Permission requested").color(p.warning).strong());
    let mut decided = None;
    let can_decide =
        backend.available(&Command::Decide { session: index, approval: pending.id.clone(), decision: Decision::Deny });
    child.horizontal_wrapped(|ui| {
        if !can_decide {
            return;
        }
        for (label, decision) in
            [("Allow", Decision::Allow), ("Allow always", Decision::AllowAlways), ("Deny", Decision::Deny)]
        {
            if request.tool == "Azure REST" && decision == Decision::AllowAlways {
                continue;
            }
            let response = ui.add_enabled(!deciding, touch::sized(egui::Button::new(label), touch));
            #[cfg(feature = "test-support")]
            ui.ctx().data_mut(|data| {
                data.insert_temp(egui::Id::unique(("approval-button", label)), response.enabled());
            });
            if response.clicked() {
                decided = Some(decision);
            }
        }
    });
    if deciding {
        child.label("Decision pending…");
    }
    egui::ScrollArea::vertical().max_height(child.available_height()).show(&mut child, |ui| {
        ui.label(RichText::new(request.command.as_deref().unwrap_or(&request.tool)).monospace());
        if !request.cwd.as_os_str().is_empty() {
            ui.label(format!("cwd: {}", request.cwd.display()));
        }
        ui.label(request.reason.as_deref().unwrap_or("Permission requested"));
    });
    if let Some(decision) = decided
        && client.approval_decisions.insert((pending.session.clone(), pending.id.clone()))
    {
        client.send(Command::Decide { session: index, approval: pending.id.clone(), decision });
    }
    body.with_min_y(banner.bottom())
}
