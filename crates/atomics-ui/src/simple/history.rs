use std::borrow::Cow;

use atomics_core::config::ChiefAgent;
use atomics_core::projection::ChiefTurn;
use atomics_core::trace::TurnRole;
use egui::Rect;

use crate::{Backend, Client};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Chat {
    agent: Option<ChiefAgent>,
    id: String,
    pub(super) title: String,
}

fn title(prompt: &str) -> String {
    let prompt = prompt.trim();
    let text = prompt
        .split_once('\n')
        .filter(|(line, _)| (line.starts_with("[Plan ") || line.starts_with("[Chat ")) && line.ends_with(']'))
        .map_or(prompt, |(_, text)| text);
    crate::markdown::plain_title(text).chars().take(90).collect()
}

pub(super) fn list(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, query: &str) -> usize {
    let mut chats = Vec::new();
    if backend.chief_history().is_empty() {
        // Paired clients can show turns their host has shared with them.
        let mut last = std::collections::HashMap::new();
        for entry in backend.trace().entries {
            let id = entry.chat_id.as_ref().unwrap_or(&entry.session_id).clone();
            if entry.turn.is_some() {
                last.insert(id.clone(), entry.at);
            }
            if let Some(turn) = entry.turn
                && turn.role == TurnRole::User
                && !turn.text.starts_with("ATOMICS ")
                && !turn.text.starts_with("[Plan ")
                && !chats.iter().any(|chat: &Chat| chat.id == id)
            {
                chats.push(Chat { agent: None, id, title: title(&turn.text) });
            }
        }
        chats.sort_by_key(|chat| last.get(&chat.id).copied().unwrap_or_default());
    } else {
        for conversation in backend.chief_history() {
            let prompt = if conversation.title.is_empty() {
                conversation
                    .turns
                    .iter()
                    .find(|t| t.speaker == "user" && !t.text.starts_with("ATOMICS "))
                    .map(|t| t.text.as_str())
            } else {
                Some(conversation.title.as_str())
            };
            if let Some(prompt) = prompt.filter(|s| !s.starts_with("[Plan ")) {
                chats.push(Chat {
                    agent: Some(conversation.agent),
                    id: conversation.conversation.clone(),
                    title: title(prompt),
                });
            }
        }
    }
    chats.retain(|chat| chat.title.to_lowercase().contains(query));
    if chats.is_empty() {
        return 0;
    }
    let ink = super::look::Ink::of(&client.palette);
    ui.label(super::look::text("Chats", super::look::SMALL, super::look::Weight::Medium, ink.secondary));
    let count = chats.len();
    for chat in chats.into_iter().rev() {
        let selected = client.simple.past_chat.as_ref().is_some_and(|s| s.id == chat.id && s.agent == chat.agent)
            || client.simple.chat_id.as_ref() == Some(&chat.id);
        let button = egui::Button::selectable(selected, "").left_text(&chat.title).truncate();
        if ui.add_sized([ui.available_width(), 30.0], button).on_hover_text(&chat.title).clicked() {
            if chat.id.starts_with("chat-") {
                super::open_chat(backend, client, chat.id);
            } else {
                client.simple.past_chat = Some(chat);
            }
            client.simple.pane = super::Pane::Chief;
            if egui::containers::menu::is_in_menu(ui) {
                egui::Popup::close_all(ui.ctx());
                ui.close();
            }
        }
    }
    count
}

pub(super) fn read(ui: &mut egui::Ui, rect: Rect, backend: &dyn Backend, client: &mut Client, chat: &Chat) {
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("chief-chat-history")
            .max_rect(rect.shrink2(egui::vec2(if crate::touch::stacks(client.touch) { 16.0 } else { 24.0 }, 16.0)))
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    child.set_clip_rect(rect);
    if child.button("Back to current chat").clicked() {
        client.simple.past_chat = None;
        return;
    }
    child.heading(&chat.title);
    let turns: Cow<'_, [ChiefTurn]> = if let Some(agent) = chat.agent {
        if let Some(url) = backend.chief_transcript_url(agent, &chat.id) {
            child.hyperlink_to("Open full transcript", url);
        }
        let stored = backend.chief_history().iter().find(|s| s.agent == agent && s.conversation == chat.id);
        Cow::Borrowed(stored.map_or(&[][..], |s| s.turns.as_slice()))
    } else {
        Cow::Owned(
            backend
                .trace()
                .entries
                .into_iter()
                .filter(|e| e.session_id == chat.id)
                .filter_map(|e| e.turn)
                .map(|t| ChiefTurn {
                    speaker: if t.role == TurnRole::User { "user" } else { "chief" }.into(),
                    text: t.text,
                })
                .collect(),
        )
    };
    egui::ScrollArea::vertical().id_salt(("saved-chief-turns", &chat.id)).auto_shrink([false, false]).show(
        &mut child,
        |ui| {
            ui.set_width(ui.available_width());
            for turn in turns.iter() {
                ui.add_space(12.0);
                if turn.speaker == "user" {
                    super::bubble(ui, &turn.text, None, client);
                    ui.add_space(10.0);
                } else {
                    super::prose::show(ui, &turn.text, &client.palette);
                }
            }
        },
    );
}
