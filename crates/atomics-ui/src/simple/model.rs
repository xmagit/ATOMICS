//! Simple mode's chief picker. The host owns availability and the session-bound switch.
use atomics_core::config::ChiefAgent;
use atomics_core::ops::HostAction;
use atomics_core::projection::{AvailableChiefs, ChiefAuth, ChiefSelection};

use super::*;

pub(super) fn provider(agent: ChiefAgent) -> &'static str {
    match agent {
        ChiefAgent::Claude => "Claude",
        ChiefAgent::Codex => "Codex",
        ChiefAgent::Opencode => "OpenCode",
    }
}

pub(super) fn auth(state: ChiefAuth) -> &'static str {
    match state {
        ChiefAuth::SignedIn => "Signed in",
        ChiefAuth::SignedOut => "Signed out",
        ChiefAuth::Unknown => "Sign-in unknown",
    }
}

fn switch(choice: &ChiefSelection, session: Option<&str>, interrupt: bool) -> Command {
    Command::Manage(HostAction::SetChiefModel {
        agent: choice.agent,
        model: choice.model.clone().unwrap_or_default(),
        expected_session: session.map(str::to_owned),
        confirm_interrupt: interrupt,
    })
}

pub(super) fn picker(
    ui: &mut egui::Ui,
    rect: Rect,
    options: &AvailableChiefs,
    session: Option<&str>,
    backend: &dyn Backend,
    client: &mut Client,
) {
    let p = client.palette.clone();
    let selected = &options.selected;
    let label = format!("{} · {}", provider(selected.agent), selected.model.as_deref().unwrap_or("CLI default"));
    let button =
        egui::Button::new(with_chevron(RichText::new(label).font(font(13.0)).color(p.foreground), p.muted, true))
            .frame(false)
            .corner_radius(6)
            .truncate();
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("simple-chief-model")
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let can = backend.available(&switch(selected, session, false)) && !options.switching;
    let mut chosen = None;
    child.add_enabled_ui(can, |ui| {
        egui::containers::menu::MenuButton::from_button(button).ui(ui, |ui| {
            ui.set_width((ui.ctx().content_rect().width() - 64.0).clamp(160.0, 300.0));
            egui::ScrollArea::vertical().max_height((ui.ctx().content_rect().height() * 0.64).min(440.0)).show(
                ui,
                |ui| {
                    for (i, row) in options.providers.iter().enumerate() {
                        if i > 0 {
                            ui.add_space(6.0);
                            ui.separator();
                        }
                        ui.label(RichText::new(provider(row.agent)).font(font(14.0)).color(p.foreground));
                        let mut detail = auth(row.auth).to_owned();
                        for (label, window) in [("session", &row.quota.session), ("weekly", &row.quota.weekly)] {
                            if let Some(w) = window {
                                detail.push_str(&format!(" · {:.0}% {label} left", w.left_percent));
                            }
                        }
                        if row.quota.session.is_none() && row.quota.weekly.is_none() {
                            detail.push_str(" · Quota unknown");
                        }
                        ui.label(RichText::new(detail).font(font(12.0)).color(p.muted));
                        if let Some(why) = &row.unavailable_reason {
                            ui.label(RichText::new(why).font(font(12.0)).color(p.muted));
                            continue;
                        }
                        if row.agent != selected.agent && session.is_some() {
                            ui.label(
                                RichText::new("Starts a new conversation with a handoff")
                                    .font(font(12.0))
                                    .color(p.muted),
                            );
                        }
                        for model in &row.models {
                            let current = row.agent == selected.agent && selected.model.as_ref() == Some(model);
                            if ui.selectable_label(current, RichText::new(model).font(font(13.5))).clicked() {
                                if !current || session.is_none() {
                                    chosen = Some(ChiefSelection { agent: row.agent, model: Some(model.clone()) });
                                }
                                ui.close();
                            }
                        }
                    }
                },
            );
        });
    });
    if let Some(choice) = chosen {
        if options.needs_confirmation
            && let Some(id) = session
        {
            client.simple.switch_chief = Some((id.to_owned(), choice));
        } else {
            client.send(switch(&choice, session, false));
        }
    }
    let Some((expected, choice)) = client.simple.switch_chief.clone() else { return };
    if session != Some(expected.as_str()) || options.switching {
        client.simple.switch_chief = None;
        return;
    }
    let modal = egui::Modal::new(egui::Id::NULL.with("simple-switch-chief")).show(ui.ctx(), |ui| {
        ui.set_width((ui.ctx().content_rect().width() - 76.0).clamp(150.0, 360.0));
        ui.label(RichText::new("Interrupt this turn?").font(font(18.0)).color(p.foreground));
        let next = format!("{} · {}", provider(choice.agent), choice.model.as_deref().unwrap_or("CLI default"));
        let detail = if choice.agent == selected.agent {
            format!("{next} will resume this conversation.")
        } else {
            format!("{next} will start a new conversation with a handoff from the current chief.")
        };
        ui.add(egui::Label::new(RichText::new(detail).font(font(14.0)).color(p.muted)).wrap());
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if pill(ui, "Cancel", false, client).clicked() {
                client.simple.switch_chief = None;
            }
            if pill(ui, "Switch chief", true, client).clicked() {
                client.send(switch(&choice, Some(&expected), true));
                client.simple.switch_chief = None;
            }
        });
    });
    if modal.should_close() {
        client.simple.switch_chief = None;
    }
}
