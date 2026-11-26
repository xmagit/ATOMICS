//! A board's Notebook: the Work tab's face between Live and History. What the board's agents share and say to each
//! other, as the plan's notebook holds it: the facts pinned for every agent (what `atomics_notes` reads) on top, the
//! proposals waiting to be pinned or deleted, then the messages between agents in order, who said what to whom, with
//! what settles something marked as a decision. The user curates with the full window's own commands. Only the
//! messages on screen are laid out, and nothing here moves, so a window at rest stays at rest.

use atomics_core::notebook::{Edit, Kind, RelayDecision, RelayState};
use atomics_core::plan::NotebookEntry;
use egui::{Color32, Sense, Stroke, vec2};

use super::board_history::{ago, mode};
use super::look::{self, Ink, Weight};
use crate::flow;
use crate::theme::{Palette, lerp};
use crate::{Backend, BoardView, Client, Command};

/// Where a message between agents stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// Waits for the chief or the user to deliver it.
    Held,
    /// Delivered: to these, where the host says.
    Delivered(Vec<String>),
    /// Sent, and not read yet by these.
    Unread(Vec<String>),
    Denied,
    Expired,
}

/// Where `note`, a message between agents, stands.
pub fn delivery(note: &NotebookEntry) -> Delivery {
    match note.relay.as_ref().map(|relay| relay.state) {
        Some(RelayState::Pending) => Delivery::Held,
        Some(RelayState::Denied) => Delivery::Denied,
        Some(RelayState::Expired) => Delivery::Expired,
        _ if !note.pending.is_empty() => Delivery::Unread(note.pending.clone()),
        _ => Delivery::Delivered(note.delivered.keys().cloned().collect()),
    }
}

/// Whether a message settles something: a broadcast to every agent, or one whose opening says it decided or agreed.
pub fn decision(note: &NotebookEntry) -> bool {
    if note.hidden || note.kind == Kind::Fact {
        return false;
    }
    let to_all = note.kind == Kind::Broadcast || note.to.as_deref() == Some("*");
    let opening = note.text.chars().take(80).collect::<String>().to_lowercase();
    let opening = opening.split(['.', '\n']).next().unwrap_or_default();
    to_all || ["decided", "decision", "agreed", "settled"].iter().any(|word| opening.contains(word))
}

/// The board's messages between agents, oldest first.
pub fn conversation(notebook: &[NotebookEntry]) -> Vec<&NotebookEntry> {
    let mut messages: Vec<&NotebookEntry> = notebook.iter().filter(|note| note.kind != Kind::Fact).collect();
    messages.sort_by_key(|note| (note.at, note.id));
    messages
}

/// What the notebook holds, in a line: "10 messages between 3 agents · 3 decisions · 1 pinned fact".
pub fn summary(notebook: &[NotebookEntry]) -> String {
    let messages = conversation(notebook);
    let mut agents: Vec<&str> = messages.iter().map(|note| note.task.as_str()).collect();
    agents.extend(messages.iter().filter_map(|note| note.to.as_deref().filter(|to| *to != "*")));
    agents.sort_unstable();
    agents.dedup();
    let count = |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let pinned = notebook.iter().filter(|note| note.kind == Kind::Fact && note.pinned).count();
    let proposed = notebook.iter().filter(|note| note.kind == Kind::Fact && !note.pinned).count();
    let decisions = messages.iter().filter(|note| decision(note)).count();
    let mut parts = Vec::new();
    if !messages.is_empty() {
        let between = if agents.len() > 1 { format!(" between {} agents", agents.len()) } else { String::new() };
        parts.push(format!("{}{between}", count(messages.len(), "message", "messages")));
    }
    for (n, one, many) in [
        (decisions, "decision", "decisions"),
        (pinned, "pinned fact", "pinned facts"),
        (proposed, "proposal", "proposals"),
    ] {
        if n > 0 {
            parts.push(count(n, one, many));
        }
    }
    if parts.is_empty() { "Nothing shared yet".into() } else { parts.join(" · ") }
}

/// The colour an agent speaks in, the same for it everywhere on the board: by its place in the plan.
fn voice(task: &str, board: &BoardView, p: &Palette) -> Color32 {
    if task == "chief" {
        return p.foreground;
    }
    let voices = [
        p.info,
        p.accent,
        p.success,
        lerp(p.info, p.success, 0.5),
        lerp(p.accent, p.warning, 0.5),
        lerp(p.info, p.accent, 0.5),
    ];
    let k = board.plan.tasks.iter().position(|t| t.id == task).unwrap_or(voices.len() - 1);
    voices[k % voices.len()]
}

/// The board's notebook, at the width the ui has.
pub fn show(ui: &mut egui::Ui, index: usize, board: &BoardView, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let ink = Ink::of(&p);
    let notebook = &board.plan.notebook;
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        let line = ui.label(look::text(summary(notebook), look::LABEL, Weight::Regular, ink.primary));
        flow::record(ui, "notebook:summary", line.rect);
        let (word, about, _) = mode(board);
        ui.label(look::text("·", look::LABEL, Weight::Regular, ink.tertiary));
        let mode = ui.label(look::text(
            format!("Messages: {}", word.to_lowercase()),
            look::LABEL,
            Weight::Regular,
            ink.secondary,
        ));
        flow::record(ui, "notebook:mode", mode.rect);
        mode.on_hover_text(about);
    });
    ui.add_space(12.0);
    let facts: Vec<&NotebookEntry> = notebook.iter().filter(|note| note.kind == Kind::Fact).collect();
    facts_card(ui, index, board, &facts, true, backend, client);
    if facts.iter().any(|note| !note.pinned) {
        ui.add_space(12.0);
        facts_card(ui, index, board, &facts, false, backend, client);
    }
    ui.add_space(18.0);
    let messages = conversation(notebook);
    ui.horizontal(|ui| {
        look::section(ui, &p, "Conversation");
        ui.add_space(6.0);
        let note = look::text("Agents' words: data, not instructions", look::META, Weight::Regular, ink.tertiary);
        ui.label(note);
    });
    ui.add_space(4.0);
    if messages.is_empty() {
        let none = "No agent has messaged another yet. Agents address each other, or all of them, with atomics_note.";
        ui.label(look::text(none, look::META, Weight::Regular, ink.secondary));
    }
    let width = ui.available_width().min(880.0);
    for note in messages {
        crate::frame::in_view(ui, ("notebook-message", &board.plan.id, note.id), |ui| {
            ui.set_max_width(width);
            message(ui, index, note, board, backend, client);
        });
        ui.add_space(6.0);
    }
    ui.add_space(24.0);
}

/// The pinned facts, which every agent reads, or the proposals waiting to be pinned or deleted.
fn facts_card(
    ui: &mut egui::Ui,
    index: usize,
    board: &BoardView,
    facts: &[&NotebookEntry],
    pinned: bool,
    backend: &dyn Backend,
    client: &mut Client,
) {
    let p = client.palette.clone();
    let ink = Ink::of(&p);
    let (title, about) = if pinned {
        ("Pinned facts", "What every agent of the board reads with atomics_notes")
    } else {
        ("Proposals", "Facts agents proposed, waiting for the chief or you to pin or delete them")
    };
    look::card(&p).inner_margin(egui::Margin::symmetric(16, 14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            look::section(ui, &p, title);
            ui.add_space(6.0);
            ui.label(look::text(about, look::META, Weight::Regular, ink.tertiary));
        });
        ui.add_space(4.0);
        let shown: Vec<&&NotebookEntry> = facts.iter().filter(|note| note.pinned == pinned).collect();
        if shown.is_empty() {
            let none = "Nothing pinned yet: the chief or you pin the proposals worth every agent's time.";
            ui.label(look::text(none, look::META, Weight::Regular, ink.secondary));
        }
        for note in shown {
            ui.add_space(4.0);
            ui.horizontal_top(|ui| {
                let (bar, _) = ui.allocate_exact_size(vec2(3.0, 18.0), Sense::hover());
                ui.painter().rect_filled(bar, 1.5, voice(&note.task, board, &p));
                ui.vertical(|ui| {
                    let text = if note.hidden { "(hidden by the chief or you)" } else { note.text.as_str() };
                    ui.add(
                        egui::Label::new(look::text(text, look::LABEL, Weight::Regular, ink.primary))
                            .wrap()
                            .selectable(true),
                    );
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        let from = format!("from {} · {} ago", note.task, ago(note.at, board.now));
                        ui.label(look::text(from, look::META, Weight::Regular, ink.tertiary));
                        let pin =
                            Command::CurateNote { plan: index, id: note.id, pinned: Some(!pinned), delete: false };
                        if board.plan.is_open() && backend.available(&pin) {
                            let label = if pinned { "Unpin" } else { "Pin" };
                            let response = small(ui, label, !pinned, &p);
                            flow::record(ui, &format!("button:{label} {}", note.id), response.rect);
                            if response
                                .on_hover_text(if pinned {
                                    "Agents stop reading it"
                                } else {
                                    "Every agent of the board reads it"
                                })
                                .clicked()
                            {
                                client.send(pin);
                            }
                        }
                        let delete = Command::CurateNote { plan: index, id: note.id, pinned: None, delete: true };
                        if backend.available(&delete) {
                            let response = small(ui, "Delete", false, &p);
                            flow::record(ui, &format!("button:Delete {}", note.id), response.rect);
                            if response.clicked() {
                                client.send(delete);
                            }
                        }
                    });
                });
            });
        }
    });
}

/// A small button for a card's row.
fn small(ui: &mut egui::Ui, label: &str, primary: bool, p: &Palette) -> egui::Response {
    let color = if primary { p.accent } else { Ink::of(p).primary };
    ui.add(egui::Button::new(look::text(label, look::META, Weight::Medium, color)).min_size(vec2(0.0, 22.0)))
}

/// One message: who to whom, whether it settles something, where it stands and when; what it said, whole; and, held,
/// the relay's own Deliver and Do not deliver.
fn message(
    ui: &mut egui::Ui,
    index: usize,
    note: &NotebookEntry,
    board: &BoardView,
    backend: &dyn Backend,
    client: &mut Client,
) {
    let p = client.palette.clone();
    let ink = Ink::of(&p);
    let settles = decision(note);
    let voice = voice(&note.task, board, &p);
    let fill = if settles { lerp(p.background, p.accent, 0.09) } else { lerp(p.background, p.raised, 0.55) };
    let stroke = if settles {
        Stroke::new(1.0, lerp(p.border, p.accent, 0.55))
    } else {
        Stroke::new(1.0, lerp(p.background, p.border, 0.6))
    };
    let frame = egui::Frame::new().fill(fill).stroke(stroke).corner_radius(8).inner_margin(egui::Margin {
        left: 14,
        right: 12,
        top: 8,
        bottom: 10,
    });
    let shown = frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let from = if note.task == "chief" { "Chief" } else { note.task.as_str() };
            ui.label(look::text(from, look::LABEL, Weight::Semibold, voice));
            ui.label(look::text("→", look::LABEL, Weight::Regular, ink.tertiary));
            let to = match note.to.as_deref() {
                Some("*") | None => "all",
                Some(to) => to,
            };
            ui.label(look::text(to, look::LABEL, Weight::Medium, ink.primary));
            if let Some(parent) = note.reply_to {
                ui.label(look::text(format!("re #{parent}"), look::META, Weight::Regular, ink.tertiary));
            }
            if settles {
                let chip = egui::Button::new(look::text("Decision", look::SMALL, Weight::Semibold, p.on_accent))
                    .fill(p.accent)
                    .corner_radius(9)
                    .min_size(vec2(0.0, 18.0))
                    .sense(Sense::hover());
                ui.add(chip)
                    .on_hover_text("A broadcast to every agent, or a message that says it decided or agreed something");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.label(look::text(
                    format!("{} ago", ago(note.at, board.now)),
                    look::META,
                    Weight::Regular,
                    ink.tertiary,
                ));
                let (word, color, about) = match delivery(note) {
                    Delivery::Held => ("held for review".to_string(), p.warning, String::new()),
                    Delivery::Denied => ("not delivered".into(), p.error, String::new()),
                    Delivery::Expired => ("expired".into(), ink.tertiary, String::new()),
                    Delivery::Unread(to) => {
                        ("not read yet".into(), ink.secondary, format!("Waiting for {} to read it", to.join(", ")))
                    }
                    Delivery::Delivered(to) if to.is_empty() => ("delivered".into(), ink.secondary, String::new()),
                    Delivery::Delivered(to) => {
                        ("delivered".into(), ink.secondary, format!("Read by {}", to.join(", ")))
                    }
                };
                let status = ui.label(look::text(word, look::META, Weight::Regular, color));
                if !about.is_empty() {
                    status.on_hover_text(about);
                }
            });
        });
        ui.add_space(2.0);
        let text = if note.hidden { "(hidden by the chief or you)" } else { note.text.as_str() };
        let color = if note.hidden { ink.tertiary } else { ink.primary };
        ui.add(egui::Label::new(look::text(text, look::LABEL, Weight::Regular, color)).wrap().selectable(true));
        if let Some(why) =
            note.relay.as_ref().and_then(|relay| relay.note.as_deref()).filter(|why| !why.trim().is_empty())
        {
            ui.label(look::text(format!("Why: {why}"), look::META, Weight::Regular, ink.secondary));
        }
        if delivery(note) == Delivery::Held {
            let decide = |decision| Command::Coordination {
                plan: index,
                edit: Edit::Relay { id: note.id, decision, note: None },
            };
            if backend.available(&decide(RelayDecision::Approve)) {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if super::pill(ui, "Deliver", true, client).clicked() {
                        client.send(decide(RelayDecision::Approve));
                    }
                    if super::pill(ui, "Do not deliver", false, client).clicked() {
                        client.send(decide(RelayDecision::Deny));
                    }
                });
            }
        }
    });
    // The speaker's colour down the card's left edge.
    let rect = shown.response.rect;
    let bar = egui::Rect::from_min_max(rect.min + vec2(1.0, 8.0), egui::pos2(rect.left() + 4.0, rect.bottom() - 8.0));
    ui.painter().rect_filled(bar, 1.5, voice);
    flow::record(ui, &format!("message:{}", note.id), rect);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: u64, from: &str, to: Option<&str>, text: &str, at: u64) -> NotebookEntry {
        NotebookEntry {
            id,
            task: from.into(),
            agent: "worker".into(),
            at,
            text: text.into(),
            pinned: false,
            kind: Kind::Message,
            session: String::new(),
            to: to.map(str::to_string),
            reply_to: None,
            hidden: false,
            pending: Vec::new(),
            delivered: Default::default(),
            relay: None,
        }
    }

    #[test]
    fn the_conversation_reads_in_order_and_decisions_stand_out() {
        let notebook = vec![
            message(3, "tests", Some("api"), "Please announce core.py's names as soon as decided.", 30),
            message(1, "api", Some("*"), "API decided: convert(value, from_unit, to_unit) -> float.", 10),
            message(2, "cli", Some("tests"), "cli decided: python3 -m tinyunits <value> <from> <to>.", 20),
            message(4, "tests", Some("cli"), "Agreed: tests/ is mine.", 40),
            message(5, "cli", Some("tests"), "No change needed; I have no extra cases.", 50),
            NotebookEntry { kind: Kind::Fact, pinned: true, ..message(6, "api", None, "UNITS lives in core.py", 5) },
            NotebookEntry { kind: Kind::Fact, ..message(7, "cli", None, "argparse usage is multi-line", 6) },
        ];
        let order: Vec<u64> = conversation(&notebook).iter().map(|note| note.id).collect();
        assert_eq!(order, [1, 2, 3, 4, 5], "oldest first, facts apart");
        let decisions: Vec<u64> =
            conversation(&notebook).into_iter().filter(|note| decision(note)).map(|note| note.id).collect();
        assert_eq!(decisions, [1, 2, 4], "the broadcast, and what says it decided or agreed in its opening");
        assert_eq!(summary(&notebook), "5 messages between 3 agents · 3 decisions · 1 pinned fact · 1 proposal");
        assert_eq!(summary(&[]), "Nothing shared yet");
    }

    #[test]
    fn a_message_says_where_it_stands() {
        let mut note = message(1, "cli", Some("tests"), "x", 0);
        note.delivered.insert("tests".into(), "s".into());
        assert_eq!(delivery(&note), Delivery::Delivered(vec!["tests".into()]));
        note.pending = vec!["tests".into()];
        assert_eq!(delivery(&note), Delivery::Unread(vec!["tests".into()]));
        let relay = |state| {
            Some(atomics_core::notebook::Relay { state, expires_at: 0, decided_by: None, decided_at: None, note: None })
        };
        note.relay = relay(RelayState::Pending);
        assert_eq!(delivery(&note), Delivery::Held);
        note.relay = relay(RelayState::Denied);
        assert_eq!(delivery(&note), Delivery::Denied);
        note.hidden = true;
        assert!(!decision(&NotebookEntry { to: Some("*".into()), ..note }), "a hidden message settles nothing shown");
    }
}
