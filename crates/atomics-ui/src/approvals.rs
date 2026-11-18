//! The banner over a session's tile while the session waits on a permission request.

use atomics_core::approvals::Decision;
use egui::{Rect, RichText};

use crate::touch;
use crate::{Backend, Client, Command};

/// Allow, Allow always (only where the request may be allowed always) and Deny, disabled while a decision is
/// pending; the one clicked. `button` makes each button from its label.
fn buttons<'a>(
    ui: &mut egui::Ui,
    request: &atomics_core::approvals::ApprovalRequest,
    deciding: bool,
    touch: Option<touch::Touch>,
    button: impl Fn(&'a str) -> egui::Button<'a>,
) -> Option<Decision> {
    let mut decided = None;
    for (label, decision) in
        [("Allow", Decision::Allow), ("Allow always", Decision::AllowAlways), ("Deny", Decision::Deny)]
    {
        if decision == Decision::AllowAlways && !request.may_allow_always() {
            continue;
        }
        let response = ui.add_enabled(!deciding, touch::sized(button(label), touch));
        #[cfg(feature = "test-support")]
        if !ui.is_sizing_pass() {
            ui.ctx().data_mut(|data| {
                data.insert_temp(egui::Id::unique(("approval-button", label)), response.enabled());
            });
        }
        if response.clicked() {
            decided = Some(decision);
        }
    }
    decided
}

/// The first permission request the session at `index` waits on, in the flow of `ui` as a card shows it: the
/// host's warning first, the command as it would run, where and why, and the banner's own buttons, which record
/// the same decision once ([`banner`]). Returns whether a request waits.
pub fn card(
    ui: &mut egui::Ui,
    index: usize,
    backend: &dyn Backend,
    client: &mut Client,
    primary: egui::Color32,
) -> bool {
    let Some(pending) = backend.session_approval(index) else { return false };
    let p = client.palette.clone();
    let request = &pending.request;
    let deciding = client.approval_decisions.contains(&(pending.session.clone(), pending.id.clone()));
    let can_decide = backend.available(&Command::Decide {
        session: index,
        approval: pending.id.clone(),
        digest: request.digest(&pending.id),
        decision: Decision::Deny,
    });
    if let Some(warning) = &request.warning {
        ui.label(RichText::new(warning).color(p.warning));
    }
    egui::Frame::new()
        .fill(p.background)
        .stroke(egui::Stroke::new(1.0, p.border))
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let command = request.command.as_deref().unwrap_or(&request.tool);
            egui::ScrollArea::vertical().id_salt(("approval-card", &pending.id)).max_height(64.0).show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(command).monospace().color(p.foreground)).wrap());
            });
        });
    let mut why = Vec::new();
    if let Some(reason) = request.reason.as_deref() {
        why.push(reason.to_string());
    }
    if !request.cwd.as_os_str().is_empty() {
        why.push(format!("in {}", request.cwd.display()));
    }
    if !why.is_empty() {
        ui.add(egui::Label::new(RichText::new(why.join(" · ")).color(p.muted).size(12.0)).wrap());
    }
    let mut decided = None;
    if can_decide {
        ui.horizontal_wrapped(|ui| {
            let on = p.on_accent;
            decided = buttons(ui, request, deciding, client.touch, |label| {
                let text = RichText::new(label).font(crate::flow::font(13.0));
                let button = egui::Button::new(text.color(if label == "Allow" { on } else { p.foreground }))
                    .corner_radius(8)
                    .min_size(egui::vec2(0.0, 28.0));
                if label == "Allow" {
                    button.fill(primary)
                } else {
                    button.fill(p.raised).stroke(egui::Stroke::new(1.0, p.border))
                }
            });
        });
    }
    if deciding {
        ui.label(RichText::new("Decision pending…").color(p.muted));
    }
    if let Some(decision) = decided
        && client.approval_decisions.insert((pending.session.clone(), pending.id.clone()))
    {
        client.send(Command::Decide {
            session: index,
            approval: pending.id.clone(),
            digest: request.digest(&pending.id),
            decision,
        });
    }
    true
}

/// The space between a desktop's banner and what it holds.
const BANNER_MARGIN: f32 = 6.0;

/// Draws the first permission request the session at `index` of [`Backend::session_list`] waits on at the
/// top of its tile's `body`, and returns the part of `body` below it; with no request waiting, `body` as it
/// is. Allow, Allow always (only where [`ApprovalRequest::may_allow_always`]) and Deny record a
/// [`Command::Decide`] once per request; the buttons stay disabled until the host has decided, and are not
/// drawn where the backend cannot decide. The host's warning, such as whose stash entries the command drops,
/// comes first.
///
/// [`ApprovalRequest::may_allow_always`]: atomics_core::approvals::ApprovalRequest::may_allow_always
pub fn banner(ui: &mut egui::Ui, body: Rect, index: usize, backend: &dyn Backend, client: &mut Client) -> Rect {
    let Some(pending) = backend.session_approval(index) else { return body };
    let p = client.palette.clone();
    let touch = client.touch;
    let request = &pending.request;
    let digest = request.digest(&pending.id);
    let deciding = client.approval_decisions.contains(&(pending.session.clone(), pending.id.clone()));
    let can_decide = backend.available(&Command::Decide {
        session: index,
        approval: pending.id.clone(),
        digest: digest.clone(),
        decision: Decision::Deny,
    });
    // The header, the buttons and what is asked. Drawn once to size it on a desktop, where the terminal gets the rest.
    let content = |child: &mut egui::Ui, decided: &mut Option<Decision>| {
        child.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Permission requested").color(p.warning).strong());
            // What asks: the tool, unless the command line below is all it is.
            if request.command.is_some() && !request.tool.is_empty() {
                ui.label(RichText::new(format!("· {}", request.tool)).color(p.muted));
            }
        });
        child.horizontal_wrapped(|ui| {
            if can_decide {
                *decided = decided.or(buttons(ui, request, deciding, touch, egui::Button::new));
            }
        });
        if deciding {
            child.label("Decision pending…");
        }
        let room = child.available_height();
        egui::ScrollArea::vertical().max_height(room).show(child, |ui| {
            // What the host warns of first (a shared stash entry, whose it is), then the command as it would run,
            // set apart and wrapped whole: nothing of it is cut.
            if let Some(warning) = &request.warning {
                ui.label(RichText::new(warning).color(p.warning));
            }
            egui::Frame::new()
                .fill(p.raised)
                .stroke(egui::Stroke::new(1.0, p.border))
                .corner_radius(crate::theme::RADIUS)
                .inner_margin(egui::Margin::symmetric(6, 4))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let command = request.command.as_deref().unwrap_or(&request.tool);
                    ui.add(egui::Label::new(RichText::new(command).monospace().color(p.foreground)).wrap());
                });
            if !request.input.is_null() && request.input != serde_json::json!({}) {
                ui.add(
                    egui::Label::new(
                        RichText::new(serde_json::to_string_pretty(&request.input).unwrap_or_default()).monospace(),
                    )
                    .wrap(),
                );
            }
            if !request.cwd.as_os_str().is_empty() {
                ui.label(RichText::new(format!("in {}", request.cwd.display())).color(p.muted).size(12.0));
            }
            if let Some(reason) = request.reason.as_deref() {
                ui.label(RichText::new(reason).color(p.foreground));
            }
        });
    };
    let most = (body.height() - 220.0).clamp(110.0, 210.0).min(body.height());
    let height = match touch {
        Some(_) => most,
        None => {
            // What it holds, up to what the terminal can spare: long requests scroll in the banner.
            let width = (body.width() - 2.0 * BANNER_MARGIN).max(0.0);
            let room = Rect::from_min_size(body.min, egui::vec2(width, 10_000.0));
            let mut probe = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(("approval-size", &pending.session))
                    .max_rect(room)
                    .sizing_pass()
                    .invisible(),
            );
            content(&mut probe, &mut None);
            (probe.min_rect().height() + 2.0 * BANNER_MARGIN).min(most)
        }
    };
    let banner = body.with_max_y(body.top() + height);
    ui.painter().rect_filled(banner, 0.0, p.panel);
    // On a phone the banner spans the screen, and its content keeps to the screen's gutter.
    let inner = touch.map_or(banner.shrink(BANNER_MARGIN), |touch| banner.shrink2(egui::vec2(touch.gutter, 8.0)));
    let mut child =
        ui.new_child(egui::UiBuilder::new().id_salt(("approval", &pending.session, &digest)).max_rect(inner));
    child.set_clip_rect(banner);
    let mut decided = None;
    content(&mut child, &mut decided);
    if let Some(decision) = decided
        && client.approval_decisions.insert((pending.session.clone(), pending.id.clone()))
    {
        client.send(Command::Decide { session: index, approval: pending.id.clone(), digest, decision });
    }
    body.with_min_y(banner.bottom())
}
