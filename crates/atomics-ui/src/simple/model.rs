//! Simple mode's chief picker. The host owns availability and the session-bound switch.
use atomics_core::config::ChiefAgent;
use atomics_core::projection::{AvailableChiefs, ChiefAuth, ChiefSelection};

use super::*;
use crate::chief::{MODEL_ACCESS, custom_codex_model, model_command};

pub(crate) fn provider(agent: ChiefAgent) -> &'static str {
    match agent {
        ChiefAgent::Claude => "Claude",
        ChiefAgent::Codex => "Codex",
        ChiefAgent::Copilot => "Copilot CLI",
        ChiefAgent::Cursor => "Cursor CLI",
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

/// The chief a new thread starts with, under "Start with fresh context": a provider, then one of its models, from what
/// the host offers (each provider's whole list). Picking a provider preselects the model the host names as its default.
/// Providers the host cannot start show their reason and cannot be picked. Off while `enabled` is not: the chief in
/// front takes it then. Whether the choice changed.
pub(crate) fn new_thread_chief(
    ui: &mut egui::Ui,
    salt: &str,
    options: &AvailableChiefs,
    choice: &mut ChiefSelection,
    enabled: bool,
    p: &Palette,
) -> bool {
    let ink = look::Ink::of(p);
    let mut changed = false;
    let words = |text: &str, color| look::text(text, look::LABEL, Weight::Regular, color);
    ui.add_enabled_ui(enabled, |ui| {
        let label = ui.label(words("Chief", ink.secondary));
        let provider_combo = crate::controls::combo((salt, "provider"))
            .width(104.0)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .selected_text(words(provider(choice.agent), ink.primary))
            .show_ui(ui, |ui| {
                ui.set_min_width(220.0);
                for row in &options.providers {
                    // Greyed out, and why: a CLI that is missing, signed out or not ready to be a chief. A click on it
                    // is taken here, so that it neither picks it nor reaches the dialog beneath and closes it.
                    if let Some(why) = &row.unavailable_reason {
                        let block = ui
                            .vertical(|ui| {
                                ui.add(egui::Label::new(words(provider(row.agent), ink.tertiary)).selectable(false));
                                let why = look::text(why, look::SMALL, Weight::Regular, ink.tertiary);
                                ui.add(egui::Label::new(why).wrap().selectable(false));
                            })
                            .response;
                        ui.interact(
                            block.rect,
                            ui.scope_id().with(("unavailable", row.agent.name())),
                            egui::Sense::click(),
                        );
                        continue;
                    }
                    let current = row.agent == choice.agent;
                    let item = egui::Button::selectable(current, words(provider(row.agent), ink.primary));
                    if ui.add(item).clicked() {
                        if !current {
                            *choice = first_choice(row);
                            changed = true;
                        }
                        ui.close();
                    }
                }
            })
            .response
            .labelled_by(label.id);
        let models = options.providers.iter().find(|row| row.agent == choice.agent).map(|row| &row.models);
        // What is left of the line, so that it ends where the fields above it do.
        let width = (ui.available_width() - ui.spacing().icon_width).clamp(110.0, 220.0);
        let model_combo = crate::controls::combo((salt, "model"))
            .width(width)
            .truncate()
            .selected_text(words(choice.model.as_deref().unwrap_or("CLI default"), ink.primary))
            .show_ui(ui, |ui| {
                for model in models.into_iter().flatten() {
                    let current = choice.model.as_ref() == Some(model);
                    if ui.add(egui::Button::selectable(current, words(model, ink.primary))).clicked() && !current {
                        choice.model = Some(model.clone());
                        changed = true;
                    }
                }
            })
            .response
            .labelled_by(label.id);
        let hint = if enabled {
            "The provider and model the new session's chief starts with"
        } else {
            "The session in front takes this, with its own chief"
        };
        flow::record(ui, "new-thread-chief", provider_combo.rect.union(model_combo.rect));
        provider_combo.on_hover_text(hint).on_disabled_hover_text(hint);
        model_combo.on_hover_text(hint).on_disabled_hover_text(hint);
    });
    changed
}

/// What picking `row`'s provider selects: the model the host names as its default where it offers it, else its first.
pub(crate) fn first_choice(row: &atomics_core::projection::ChiefProvider) -> ChiefSelection {
    let model =
        row.default_model.clone().filter(|model| row.models.contains(model)).or_else(|| row.models.first().cloned());
    ChiefSelection { agent: row.agent, model }
}

pub(super) fn picker(
    ui: &mut egui::Ui,
    rect: Rect,
    options: &AvailableChiefs,
    session: Option<&str>,
    active_thread: bool,
    backend: &dyn Backend,
    client: &mut Client,
) {
    let p = client.palette.clone();
    let selected = &options.selected;
    let label = format!("{} · {}", provider(selected.agent), selected.model.as_deref().unwrap_or("CLI default"));
    let button =
        egui::Button::new(with_chevron(RichText::new(label).font(font(13.0)).color(p.foreground), p.muted, true))
            .frame_when_inactive(false)
            .corner_radius(6)
            .truncate();
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("simple-chief-model")
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let can_change = active_thread && backend.available(&model_command(selected, session, false));
    let mut custom = false;
    let mut chosen = None;
    child.add_enabled_ui(!options.switching, |ui| {
        egui::containers::menu::MenuButton::from_button(button).ui(ui, |ui| {
            ui.set_width((ui.ctx().content_rect().width() - 64.0).clamp(160.0, 300.0));
            egui::ScrollArea::vertical().max_height((ui.ctx().content_rect().height() * 0.64).min(440.0)).show(
                ui,
                |ui| {
                    if !can_change {
                        ui.label(MODEL_ACCESS);
                    }
                    let rows = options
                        .providers
                        .iter()
                        .filter(|row| row.agent == selected.agent)
                        .chain(options.providers.iter().filter(|row| row.agent != selected.agent));
                    for (i, row) in rows.enumerate() {
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
                        if let Some(budget) = &row.quota.budget {
                            detail.push_str(&format!(
                                " · {:.0}% {} left",
                                budget.left_percent,
                                budget.label.to_lowercase()
                            ));
                        }
                        if row.quota.session.is_none() && row.quota.weekly.is_none() && row.quota.budget.is_none() {
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
                        if row.agent == ChiefAgent::Codex
                            && ui.add_enabled(can_change, egui::Button::new("Custom model…")).clicked()
                        {
                            custom = true;
                            ui.close();
                        }
                        for model in &row.models {
                            let current = row.agent == selected.agent && selected.model.as_ref() == Some(model);
                            let item = egui::Button::selectable(current, RichText::new(model).font(font(13.5)));
                            if ui.add_enabled(can_change, item).clicked() {
                                chosen = Some(ChiefSelection { agent: row.agent, model: Some(model.clone()) });
                                ui.close();
                            }
                        }
                    }
                },
            );
        });
    });
    let custom = custom_codex_model(ui, session, custom, can_change && !options.switching);
    if let Some(choice) = chosen.or(custom)
        && (choice != *selected || session.is_none())
    {
        if options.needs_confirmation
            && let Some(id) = session
        {
            client.simple.switch_chief = Some((id.to_owned(), choice));
        } else {
            client.send(model_command(&choice, session, false));
        }
    }
    let Some((expected, choice)) = client.simple.switch_chief.clone() else { return };
    if session != Some(expected.as_str()) || options.switching || !can_change {
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
                client.send(model_command(&choice, Some(&expected), true));
                client.simple.switch_chief = None;
            }
        });
    });
    if modal.should_close() {
        client.simple.switch_chief = None;
    }
}
