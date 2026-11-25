//! The banner over a session's tile while the session waits on a permission request.

use atomics_core::approvals::Decision;
use egui::{Rect, RichText};

use crate::{Backend, Client, Command};
use crate::{icons, touch};

/// Allow, Allow always (only where the request may be allowed always) and Deny, disabled while a decision is
/// pending; the one clicked. `button` makes each button from its label and icon.
fn buttons<'a>(
    ui: &mut egui::Ui,
    request: &atomics_core::approvals::ApprovalRequest,
    deciding: bool,
    touch: Option<touch::Touch>,
    button: impl Fn(&'a str, &'static str) -> egui::Button<'a>,
) -> Option<Decision> {
    let mut decided = None;
    for (label, icon, decision) in [
        ("Allow", icons::APPROVE, Decision::Allow),
        ("Allow always", icons::APPROVE_ALWAYS, Decision::AllowAlways),
        ("Deny", icons::CLOSE, Decision::Deny),
    ] {
        if decision == Decision::AllowAlways && !request.may_allow_always() {
            continue;
        }
        let response = ui.add_enabled(!deciding, touch::sized(button(label, icon), touch));
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

/// Render the fields the host provided, including unfamiliar arguments from other MCP servers.
pub(crate) fn request_fields(ui: &mut egui::Ui, input: &serde_json::Value, p: &crate::theme::Palette) {
    use crate::simple::look::{self, Weight};
    let ink = look::Ink::of(p);
    let value = |ui: &mut egui::Ui, v: &serde_json::Value| {
        let text = v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string());
        ui.add(egui::Label::new(look::text(text, look::LABEL, Weight::Regular, ink.primary)).wrap());
    };
    if let Some(fields) = input.as_object() {
        for (key, v) in fields {
            let label = match key.as_str() {
                "id" => "ID".into(),
                "plan" => "Board".into(),
                key => {
                    let mut chars = key.chars();
                    chars
                        .next()
                        .map(|c| c.to_uppercase().to_string() + &chars.as_str().replace('_', " "))
                        .unwrap_or_default()
                }
            };
            ui.horizontal_top(|ui| {
                ui.add_sized(
                    egui::vec2(74.0, 18.0),
                    egui::Label::new(look::text(label, look::SMALL, Weight::Semibold, ink.secondary)).wrap(),
                );
                value(ui, v);
            });
        }
    } else {
        value(ui, input);
    }
}

/// A pending action in the conversation or Needs you inbox, bound to the host's exact request and digest.
pub fn card(
    ui: &mut egui::Ui,
    index: usize,
    pending: &crate::ApprovalRow,
    requester: &str,
    backend: &dyn Backend,
    client: &mut Client,
) {
    use crate::simple::look::{self, Weight};
    let p = client.palette.clone();
    let ink = look::Ink::of(&p);
    let request = &pending.request;
    let digest = request.digest(&pending.id);
    let deciding = client.approval_decisions.contains(&(pending.session.clone(), pending.id.clone()));
    let can_decide = backend.available(&Command::Decide {
        session: index,
        approval: pending.id.clone(),
        digest: digest.clone(),
        decision: Decision::Deny,
    });
    let start = matches!(request.tool.as_str(), "mcp__atomics__atomics_start" | "atomics_start" | "MCP:atomics_start");
    let title = if start {
        request
            .input
            .get("agent")
            .and_then(|a| a.as_str())
            .and_then(atomics_core::config::ChiefAgent::from_name)
            .map(|agent| format!("Start a {} agent", crate::simple::model::provider(agent)))
            .unwrap_or_else(|| "Start an agent".into())
    } else if request.command.is_some() {
        "Run a command".into()
    } else {
        format!("Use {}", request.tool)
    };
    let mut decision = None;
    ui.push_id(("simple-permission", &pending.session, &pending.id, &digest), |ui| {
        look::card(&p).inner_margin(egui::Margin::symmetric(16, 12)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                if let Some(icon) = icons::text(icons::NEEDS_YOU, 16.0) {
                    ui.label(icon.color(p.warning));
                }
                ui.label(look::text("Permission requested", look::SMALL, Weight::Semibold, ink.primary));
                ui.label(look::text(requester, look::SMALL, Weight::Regular, ink.secondary));
            });
            ui.add_space(3.0);
            ui.add(egui::Label::new(look::text(title, look::TITLE, Weight::Semibold, ink.primary)).wrap());
            if let Some(warning) = &request.warning {
                ui.add(
                    egui::Label::new(look::text(warning, look::LABEL, Weight::Semibold, p.simple_text(p.warning, 4.6)))
                        .wrap(),
                );
            }
            if let Some(reason) = request.reason.as_deref() {
                ui.add(egui::Label::new(look::text(reason, look::LABEL, Weight::Regular, ink.secondary)).wrap());
            }
            if let Some(command) = request.command.as_deref() {
                egui::Frame::new().fill(p.background).corner_radius(5).inner_margin(8).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical().id_salt("command").max_height(100.0).show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                RichText::new(command).font(crate::theme::simple_mono(look::CODE)).color(ink.primary),
                            )
                            .wrap(),
                        );
                    });
                });
            }
            if start
                && let Some(prompt) = request.input.get("prompt").and_then(|s| s.as_str()).filter(|s| !s.is_empty())
            {
                ui.add(egui::Label::new(look::text(prompt, look::LABEL, Weight::Regular, ink.primary)).truncate())
                    .on_hover_text(prompt);
            }
            let folder = if start {
                request.input.get("folder").and_then(|s| s.as_str()).map(std::path::Path::new)
            } else {
                None
            };
            let folder = folder.unwrap_or(request.cwd.as_path());
            if !folder.as_os_str().is_empty() {
                ui.add(
                    egui::Label::new(look::text(
                        folder.display().to_string(),
                        look::LABEL,
                        Weight::Regular,
                        ink.secondary,
                    ))
                    .wrap(),
                );
            }
            egui::CollapsingHeader::new(look::text("Request details", look::LABEL, Weight::Medium, ink.secondary))
                .default_open(!start && request.command.is_none())
                .show(ui, |ui| {
                    ui.label(look::text(&request.tool, look::LABEL, Weight::Semibold, ink.primary));
                    if !request.cwd.as_os_str().is_empty() && folder != request.cwd {
                        ui.label(request.cwd.display().to_string());
                    }
                    if !request.input.is_null() && request.input != serde_json::json!({}) {
                        request_fields(ui, &request.input, &p);
                    }
                });
            if can_decide {
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().button_padding = egui::vec2(11.0, 5.0);
                    decision = buttons(ui, request, deciding, client.touch, |label, _| {
                        let once = label == "Allow";
                        let name = if once { "Allow once" } else { label };
                        let text = look::text(
                            name,
                            look::LABEL,
                            Weight::Semibold,
                            if once { p.on_accent } else { ink.primary },
                        );
                        let button = egui::Button::new(text).min_size(egui::vec2(0.0, 34.0)).corner_radius(6);
                        if once { button.fill(p.accent) } else { button }
                    });
                });
            }
            if deciding {
                ui.label(look::text("Decision pending…", look::LABEL, Weight::Medium, ink.secondary));
            }
        });
    });
    if let Some(decision) = decision
        && client.approval_decisions.insert((pending.session.clone(), pending.id.clone()))
    {
        client.send(Command::Decide { session: index, approval: pending.id.clone(), digest, decision });
    }
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
            if let Some(icon) = icons::text(icons::NEEDS_YOU, icons::SIZE) {
                ui.label(icon.color(p.warning));
            }
            ui.label(RichText::new("Permission requested").color(p.warning).strong());
            // What asks: the tool, unless the command line below is all it is.
            if request.command.is_some() && !request.tool.is_empty() {
                ui.label(RichText::new(format!("· {}", request.tool)).color(p.muted));
            }
        });
        child.horizontal_wrapped(|ui| {
            if can_decide {
                *decided = decided.or(buttons(ui, request, deciding, touch, |label, icon| {
                    egui::Button::new(icons::with(icon, label))
                }));
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
