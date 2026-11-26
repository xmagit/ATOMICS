use std::borrow::Cow;

use atomics_core::config::ChiefAgent;
use atomics_core::projection::ChiefTurn;
use atomics_core::trace::TurnRole;
use egui::{Color32, Rect, Sense, pos2, vec2};

use super::look::{self, Weight};
use crate::theme::{Palette, lerp};

use crate::{Backend, Client};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Chat {
    agent: Option<ChiefAgent>,
    id: String,
    pub(super) title: String,
}

/// A chat's title from its first prompt, without the `[Plan …]` or `[Chat …]` scope ATOMICS put before it, on its
/// own line or not.
pub(super) fn title(prompt: &str) -> String {
    let prompt = prompt.trim();
    let text = ["[Plan ", "[Chat "]
        .iter()
        .any(|scope| prompt.starts_with(scope))
        .then(|| prompt.split_once(']'))
        .flatten()
        .map_or(prompt, |(_, text)| text.trim_start());
    crate::markdown::plain_title(text).chars().take(90).collect()
}

pub(super) fn session_title(backend: &dyn Backend, client: &Client, id: &str) -> String {
    let history = backend.chief_history();
    let chat = history.iter().find(|s| s.conversation == id);
    chat.filter(|s| !s.title.is_empty())
        .map(|s| title(&s.title))
        .filter(|title| !title.is_empty())
        // A chat that planned a board without a title of its own is named after it.
        .or_else(|| {
            let plan = chat?.plan.as_deref()?;
            backend.plans().iter().find(|row| row.id == plan).map(|row| super::goal(&row.goal))
        })
        .or_else(|| client.simple.session_titles.get(id).cloned())
        .unwrap_or_else(|| "Untitled chat".into())
}

pub(super) enum Mark {
    Chief(Option<ChiefAgent>, Color32, bool),
    Board(Color32),
    Finished,
    Chat,
}

/// A session in the rail or history browser. The icon and detail share the same columns as the plan rows.
pub(super) fn session_row(
    ui: &mut egui::Ui,
    p: &Palette,
    title: &str,
    detail: &str,
    mark: Mark,
    selected: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), super::PLAN_ROW), Sense::click());
    let painter = ui.painter().clone();
    let ink = look::Ink::of(p);
    if selected {
        painter.rect_filled(rect, 4.0, lerp(p.sidebar, p.accent, 0.12));
        painter.rect_filled(rect.shrink2(vec2(0.0, 5.0)).with_max_x(rect.left() + 2.0), 0.0, p.accent);
    }
    crate::controls::feedback(ui, rect, &response, 4.0, p);
    let center = pos2(rect.left() + 16.0, rect.top() + 16.0);
    let mut detail_left = rect.left() + 32.0;
    match mark {
        Mark::Chief(provider, status, animate) => {
            crate::chief::provider_icon(
                &crate::status::chief_icon_painter(ui, &painter, animate),
                Rect::from_center_size(center, vec2(18.0, 18.0)),
                provider,
                p.accent,
                ink.primary,
            );
            painter.circle_filled(pos2(detail_left + 3.0, rect.top() + 35.0), 3.0, status);
            detail_left += 12.0;
        }
        Mark::Board(color) => {
            crate::icons::paint(&painter, center, crate::icons::BOARD, 17.0, color);
        }
        Mark::Finished => {
            crate::icons::paint(&painter, center, crate::icons::DONE, 16.0, ink.secondary);
        }
        Mark::Chat => {
            crate::icons::paint(&painter, center, crate::icons::CHAT, 17.0, ink.secondary);
        }
    }
    let left = rect.left() + 32.0;
    let title_rect = Rect::from_min_max(pos2(left, rect.top()), pos2(rect.right() - 6.0, rect.center().y + 3.0));
    let mut line = ui.new_child(
        egui::UiBuilder::new().max_rect(title_rect).layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    line.add(
        egui::Label::new(look::text(title, look::LABEL, Weight::Semibold, ink.primary))
            .truncate()
            .selectable(false)
            .sense(Sense::hover()),
    );
    let detail_galley = crate::flow::truncated(
        &painter,
        detail,
        super::font(look::META),
        ink.secondary,
        (rect.right() - detail_left - 6.0).max(1.0),
    );
    painter.galley(pos2(detail_left, rect.top() + 35.0 - detail_galley.size().y / 2.0), detail_galley, ink.secondary);
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::Role::RadioButton, true, selected, format!("{title}, {detail}"))
    });
    // One tooltip a row: its callers add it, with whatever else it should hold.
    response
}

pub(super) fn list(
    ui: &mut egui::Ui,
    backend: &dyn Backend,
    client: &mut Client,
    query: &str,
    include_open: bool,
) -> usize {
    let mut chats = Vec::new();
    let history = backend.chief_history();
    if history.is_empty() {
        // Paired clients can show turns their host has shared with them.
        let mut last = std::collections::HashMap::new();
        for entry in &backend.shared_trace().entries {
            let id = entry.chat_id.as_ref().unwrap_or(&entry.session_id).clone();
            if entry.turn.is_some() {
                last.insert(id.clone(), entry.at);
            }
            if let Some(turn) = &entry.turn
                && turn.role == TurnRole::User
                && !atomics_core::trace::is_note(&turn.text)
                && !turn.text.starts_with("[Plan ")
                && !chats.iter().any(|chat: &Chat| chat.id == id)
            {
                chats.push(Chat { agent: None, id, title: title(&turn.text) });
            }
        }
        chats.sort_by_key(|chat| last.get(&chat.id).copied().unwrap_or_default());
    } else {
        for conversation in history {
            if conversation.plan.is_some() {
                continue;
            }
            let prompt = if conversation.title.is_empty() {
                conversation
                    .turns
                    .iter()
                    .find(|t| t.speaker == "user" && !atomics_core::trace::is_note(&t.text))
                    .map(|t| t.text.as_str())
            } else {
                Some(conversation.title.as_str())
            };
            if prompt.is_none() && conversation.conversation.starts_with("chat-") {
                chats.push(Chat {
                    agent: Some(conversation.agent),
                    id: conversation.conversation.clone(),
                    title: "Untitled chat".into(),
                });
            }
            if let Some(prompt) = prompt.filter(|s| !s.starts_with("[Plan ")) {
                chats.push(Chat {
                    agent: Some(conversation.agent),
                    id: conversation.conversation.clone(),
                    title: title(prompt),
                });
            }
        }
    }
    for id in &client.simple.sessions {
        if !chats.iter().any(|chat| chat.id == *id)
            && !history.iter().any(|s| &s.conversation == id && s.plan.is_some())
        {
            let title = client
                .simple
                .drafts
                .get(id)
                .filter(|draft| !draft.trim().is_empty())
                .map(|draft| title(draft))
                .unwrap_or_else(|| session_title(backend, client, id));
            chats.push(Chat { agent: None, id: id.clone(), title });
        }
    }
    chats.retain(|chat| {
        chat.title.to_lowercase().contains(query)
            && (include_open
                || !atomics_core::trace::valid_chat_id(&chat.id)
                || (!client.simple.sessions.contains(&chat.id) && !history.iter().any(|c| c.conversation == chat.id)))
    });
    if chats.is_empty() {
        return 0;
    }
    let palette = client.palette.clone();
    ui.label(look::text("Chats", look::LABEL, Weight::Semibold, look::Ink::of(&palette).secondary));
    let count = chats.len();
    for chat in chats.into_iter().rev() {
        if !super::rail_row_visible(ui, super::PLAN_ROW) {
            continue;
        }
        let selected = client.simple.past_chat.as_ref().is_some_and(|s| s.id == chat.id && s.agent == chat.agent)
            || client.simple.chat_id.as_ref() == Some(&chat.id);
        let provider = if client.simple.drafts.get(&chat.id).is_some_and(|draft| !draft.is_empty()) {
            "Draft"
        } else {
            chat.agent.map(super::model::provider).unwrap_or("Workspace chat")
        };
        let clicked = ui
            .push_id((chat.agent.map(ChiefAgent::name), &chat.id), |ui| {
                session_row(ui, &palette, &chat.title, provider, Mark::Chat, selected)
                    .on_hover_text(&chat.title)
                    .clicked()
            })
            .inner;
        if clicked {
            if chat.id.starts_with("chat-") {
                if chat.title != "Untitled chat" {
                    client.simple.session_titles.entry(chat.id.clone()).or_insert(chat.title);
                }
                super::open_chat(backend, client, Some(chat.id));
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
                    super::prose::reply(ui, &turn.text, &client.palette);
                }
            }
        },
    );
}
