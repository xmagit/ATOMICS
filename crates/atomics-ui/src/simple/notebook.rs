//! A board's Notebook: the Work tab's face between Live and History. What the board's agents share and say to each
//! other, as the plan's notebook holds it: the facts pinned for every agent (what `atomics_notes` reads) on top, the
//! proposals waiting to be pinned or deleted, then the messages between agents as threads: two agents both ways, or one
//! agent's broadcasts to all, newest activity first, replies under what they answer, each folding to a line, with what
//! settles something marked as a decision. The user curates with the full window's own commands. Only the
//! messages on screen are laid out, and nothing here moves, so a window at rest stays at rest.

use atomics_core::notebook::{Edit, Kind, RelayDecision, RelayState};
use atomics_core::plan::NotebookEntry;
use egui::{Color32, Sense, Stroke, vec2};

use super::board_history::{ago, mode};
use super::look::{self, Ink, Weight};
use crate::flow::{self, font};
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
    /// Reached nobody: every recipient had finished when it was sent.
    Finished(Vec<String>),
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
        _ if note.delivered.is_empty() && !note.undelivered.is_empty() => {
            Delivery::Finished(note.undelivered.keys().cloned().collect())
        }
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

/// A thread of the conversation: two agents both ways, or one agent's broadcasts to every agent, with the replies to
/// either under what they answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Thread<'a> {
    /// Stable across reads, for what is folded: "a⇄b", the pair in order, or "a→all".
    pub key: String,
    pub from: String,
    /// The other agent; `None` for broadcasts.
    pub with: Option<String>,
    /// Its messages in order, each reply right after what it answers, with how deep it nests.
    pub messages: Vec<(&'a NotebookEntry, usize)>,
    /// When its newest message came, by the host's clock.
    pub latest: u64,
}

impl Thread<'_> {
    /// How many of its messages settle something.
    pub fn decisions(&self) -> usize {
        self.messages.iter().filter(|(note, _)| decision(note)).count()
    }

    /// How many of its messages their recipients have not read yet.
    pub fn unread(&self) -> usize {
        self.messages.iter().filter(|(note, _)| matches!(delivery(note), Delivery::Unread(_))).count()
    }

    /// How many wait for the chief or the user to deliver them.
    pub fn held(&self) -> usize {
        self.messages.iter().filter(|(note, _)| delivery(note) == Delivery::Held).count()
    }

    /// Whether it starts open: it holds a message that starts open.
    pub fn starts_open(&self) -> bool {
        self.messages.iter().any(|(note, _)| starts_open(note))
    }
}

/// Whether a message shows whole before anyone opens it: a decision, one not read yet, or one held for review.
pub fn starts_open(note: &NotebookEntry) -> bool {
    decision(note) || matches!(delivery(note), Delivery::Unread(_) | Delivery::Held)
}

/// The conversation as threads, newest activity first. A reply joins the thread of what it answers, whoever sent it.
pub fn threads(notebook: &[NotebookEntry]) -> Vec<Thread<'_>> {
    let messages = conversation(notebook);
    let by_id: std::collections::HashMap<u64, &NotebookEntry> = messages.iter().map(|note| (note.id, *note)).collect();
    let parent =
        |note: &NotebookEntry| note.reply_to.and_then(|id| by_id.get(&id).copied()).filter(|p| p.id != note.id);
    let key = |note: &NotebookEntry| match note.to.as_deref() {
        Some(to) if to != "*" && note.kind != Kind::Broadcast => {
            let (a, b) = if note.task.as_str() <= to { (note.task.as_str(), to) } else { (to, note.task.as_str()) };
            (format!("{a}⇄{b}"), a.to_string(), Some(b.to_string()))
        }
        _ => (format!("{}→all", note.task), note.task.clone(), None),
    };
    let mut threads: Vec<Thread> = Vec::new();
    for note in &messages {
        if parent(note).is_some() {
            continue;
        }
        let (key, from, with) = key(note);
        if !threads.iter().any(|thread| thread.key == key) {
            threads.push(Thread { key, from, with, messages: Vec::new(), latest: 0 });
        }
    }
    // Each root in order, then the replies under it, depth first and oldest first.
    fn place<'a>(
        note: &'a NotebookEntry,
        depth: usize,
        all: &[&'a NotebookEntry],
        into: &mut Vec<(&'a NotebookEntry, usize)>,
    ) {
        into.push((note, depth));
        for reply in all.iter().filter(|r| r.reply_to == Some(note.id) && r.id != note.id) {
            if depth < 16 {
                place(reply, depth + 1, all, into);
            }
        }
    }
    for note in messages.iter().filter(|note| parent(note).is_none()) {
        let (key, ..) = key(note);
        let thread = threads.iter_mut().find(|thread| thread.key == key).expect("made above");
        place(note, 0, &messages, &mut thread.messages);
    }
    for thread in &mut threads {
        thread.latest = thread.messages.iter().map(|(note, _)| note.at).max().unwrap_or(0);
    }
    threads.sort_by(|a, b| b.latest.cmp(&a.latest).then(a.key.cmp(&b.key)));
    threads
}

/// What is open in a board's conversation, where someone opened or folded it; the rest is as it starts.
#[derive(Debug, Clone, Default)]
struct Folds {
    threads: std::collections::HashMap<String, bool>,
    messages: std::collections::HashMap<u64, bool>,
}

impl Folds {
    fn id(plan: &str) -> egui::Id {
        egui::Id::unique(("notebook-folds", plan))
    }

    fn of(ctx: &egui::Context, plan: &str) -> Folds {
        ctx.data(|d| d.get_temp::<Folds>(Self::id(plan))).unwrap_or_default()
    }

    fn keep(self, ctx: &egui::Context, plan: &str) {
        ctx.data_mut(|d| d.insert_temp(Self::id(plan), self));
    }

    fn thread_open(&self, thread: &Thread) -> bool {
        self.threads.get(&thread.key).copied().unwrap_or_else(|| thread.starts_open())
    }

    fn message_open(&self, note: &NotebookEntry) -> bool {
        self.messages.get(&note.id).copied().unwrap_or_else(|| starts_open(note))
    }
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
    let threads = threads(notebook);
    let plan = board.plan.id.as_str();
    let mut folds = Folds::of(ui.ctx(), plan);
    ui.horizontal(|ui| {
        look::section(ui, &p, "Conversation");
        ui.add_space(6.0);
        let note = look::text("Agents' words: data, not instructions", look::META, Weight::Regular, ink.tertiary);
        ui.label(note);
        if !threads.is_empty() {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                for (label, open) in [("Collapse all", false), ("Expand all", true)] {
                    let text = look::text(label, look::META, Weight::Medium, ink.secondary);
                    let response = ui.add(egui::Button::new(text).frame_when_inactive(false));
                    flow::record(ui, &format!("button:{label}"), response.rect);
                    if response.clicked() {
                        for thread in &threads {
                            folds.threads.insert(thread.key.clone(), open);
                            for (note, _) in &thread.messages {
                                folds.messages.insert(note.id, open);
                            }
                        }
                    }
                }
            });
        }
    });
    ui.add_space(4.0);
    if threads.is_empty() {
        let none = "No agent has messaged another yet. Agents address each other, or all of them, with atomics_note.";
        ui.label(look::text(none, look::META, Weight::Regular, ink.secondary));
    }
    let width = ui.available_width().min(880.0);
    for thread in &threads {
        crate::frame::in_view(ui, ("notebook-thread", plan, &thread.key), |ui| {
            ui.set_max_width(width);
            thread_card(ui, index, thread, &mut folds, board, backend, client);
        });
        ui.add_space(8.0);
    }
    folds.keep(ui.ctx(), plan);
    ui.add_space(24.0);
}

/// One thread: its agents, how many messages and what matters among them, and when it last spoke; folded, its latest
/// message's first line; open, its messages, each a line until opened.
fn thread_card(
    ui: &mut egui::Ui,
    index: usize,
    thread: &Thread,
    folds: &mut Folds,
    board: &BoardView,
    backend: &dyn Backend,
    client: &mut Client,
) {
    let p = client.palette.clone();
    let ink = Ink::of(&p);
    let open = folds.thread_open(thread);
    let frame = egui::Frame::new()
        .fill(lerp(p.background, p.raised, 0.35))
        .stroke(Stroke::new(1.0, lerp(p.background, p.border, 0.7)))
        .corner_radius(10)
        .inner_margin(egui::Margin { left: 10, right: 10, top: 4, bottom: if open { 8 } else { 4 } });
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
        flow::record(ui, &format!("thread:{}", thread.key), rect);
        crate::controls::feedback(ui, rect, &response, 6.0, &p);
        let painter = ui.painter_at(rect);
        let y = rect.center().y;
        chevron(&painter, egui::pos2(rect.left() + 8.0, y), open, ink.tertiary);
        let mut x = rect.left() + 20.0;
        let name = |task: &str| if task == "chief" { "Chief".to_string() } else { task.to_string() };
        let from = look::paint(
            &painter,
            egui::pos2(x, y),
            egui::Align2::LEFT_CENTER,
            &name(&thread.from),
            look::LABEL,
            Weight::Semibold,
            voice(&thread.from, board, &p),
        );
        x = from.right() + 6.0;
        let (link, other, color) = match &thread.with {
            Some(with) => ("⇄", name(with), voice(with, board, &p)),
            None => ("→", "all".to_string(), ink.primary),
        };
        let arrow = look::paint(
            &painter,
            egui::pos2(x, y),
            egui::Align2::LEFT_CENTER,
            link,
            look::LABEL,
            Weight::Regular,
            ink.tertiary,
        );
        let to = look::paint(
            &painter,
            egui::pos2(arrow.right() + 6.0, y),
            egui::Align2::LEFT_CENTER,
            &other,
            look::LABEL,
            Weight::Semibold,
            color,
        );
        x = to.right() + 8.0;
        let mut counts = vec![(format!("· {}", thread.messages.len()), ink.tertiary)];
        for (n, one, many, color) in [
            (thread.decisions(), "decision", "decisions", p.accent),
            (thread.held(), "held", "held", p.warning),
            (thread.unread(), "not read yet", "not read yet", ink.secondary),
        ] {
            if n > 0 {
                counts.push((format!("{n} {}", if n == 1 { one } else { many }), color));
            }
        }
        for (words, color) in counts {
            let shown = look::paint(
                &painter,
                egui::pos2(x, y),
                egui::Align2::LEFT_CENTER,
                &words,
                look::META,
                Weight::Medium,
                color,
            );
            x = shown.right() + 8.0;
        }
        let when = look::paint(
            &painter,
            egui::pos2(rect.right() - 4.0, y),
            egui::Align2::RIGHT_CENTER,
            &format!("{} ago", ago(thread.latest, board.now)),
            look::META,
            Weight::Regular,
            ink.tertiary,
        );
        // Folded, the latest message's first line after the counts, where there is room.
        if !open && let Some((latest, _)) = thread.messages.iter().max_by_key(|(note, _)| (note.at, note.id)) {
            let room = when.left() - 12.0 - x;
            if room > 60.0 {
                let line = first_line(latest);
                let galley = flow::truncated(&painter, &line, font(look::META), ink.secondary, room);
                painter.galley(egui::pos2(x + 4.0, y - galley.size().y / 2.0), galley, ink.secondary);
            }
        }
        if response.clicked() {
            folds.threads.insert(thread.key.clone(), !open);
        }
        if !open {
            return;
        }
        for (note, depth) in &thread.messages {
            ui.add_space(4.0);
            ui.horizontal_top(|ui| {
                // A reply sits under what it answers, a step in for each level.
                ui.add_space((*depth).min(4) as f32 * 18.0);
                ui.vertical(|ui| {
                    if folds.message_open(note) {
                        if message(ui, index, note, board, backend, client) {
                            folds.messages.insert(note.id, false);
                        }
                    } else if message_line(ui, note, board, &p) {
                        folds.messages.insert(note.id, true);
                    }
                });
            });
        }
    });
}

/// A message's first line, as a folded line shows it.
fn first_line(note: &NotebookEntry) -> String {
    if note.hidden {
        return "(hidden by the chief or you)".into();
    }
    note.text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or_default().to_string()
}

/// A small chevron, drawn: down for open, right for folded.
fn chevron(painter: &egui::Painter, c: egui::Pos2, open: bool, color: Color32) {
    let points = if open {
        vec![c + vec2(-3.5, -1.5), c + vec2(0.0, 2.0), c + vec2(3.5, -1.5)]
    } else {
        vec![c + vec2(-1.5, -3.5), c + vec2(2.0, 0.0), c + vec2(-1.5, 3.5)]
    };
    painter.add(egui::Shape::line(points, Stroke::new(1.3, color)));
}

/// A message folded to one line: who to whom, whether it settles something, its first line, and when. Returns
/// whether it was clicked, to open it.
fn message_line(ui: &mut egui::Ui, note: &NotebookEntry, board: &BoardView, p: &Palette) -> bool {
    let ink = Ink::of(p);
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::click());
    flow::record(ui, &format!("message:{}", note.id), rect);
    crate::controls::feedback(ui, rect, &response, 6.0, p);
    let painter = ui.painter_at(rect);
    let y = rect.center().y;
    let voice = voice(&note.task, board, p);
    painter.rect_filled(
        egui::Rect::from_min_max(egui::pos2(rect.left() + 2.0, y - 7.0), egui::pos2(rect.left() + 5.0, y + 7.0)),
        1.5,
        voice,
    );
    let from = if note.task == "chief" { "Chief" } else { note.task.as_str() };
    let mut x = look::paint(
        &painter,
        egui::pos2(rect.left() + 12.0, y),
        egui::Align2::LEFT_CENTER,
        from,
        look::META,
        Weight::Semibold,
        voice,
    )
    .right()
        + 8.0;
    if decision(note) {
        x = look::paint(
            &painter,
            egui::pos2(x, y),
            egui::Align2::LEFT_CENTER,
            "Decision",
            look::META,
            Weight::Semibold,
            p.accent,
        )
        .right()
            + 8.0;
    }
    let when = look::paint(
        &painter,
        egui::pos2(rect.right() - 4.0, y),
        egui::Align2::RIGHT_CENTER,
        &format!("{} ago", ago(note.at, board.now)),
        look::META,
        Weight::Regular,
        ink.tertiary,
    );
    let room = when.left() - 12.0 - x;
    if room > 40.0 {
        let galley = flow::truncated(&painter, &first_line(note), font(look::META), ink.primary, room);
        painter.galley(egui::pos2(x, y - galley.size().y / 2.0), galley, ink.primary);
    }
    response.on_hover_text("Show the whole message").clicked()
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
                        let mut from = format!("from {} · {} ago", note.task, ago(note.at, board.now));
                        if let Some(message) = note.pinned_from {
                            from.push_str(&format!(" · message #{message}"));
                        }
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
) -> bool {
    let mut fold = false;
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
                let folding = look::text("Fold", look::META, Weight::Medium, ink.tertiary);
                let response = ui.add(egui::Button::new(folding).frame_when_inactive(false));
                flow::record(ui, &format!("fold:{}", note.id), response.rect);
                fold = response.on_hover_text("Show it as one line").clicked();
                ui.label(look::text(
                    format!("{} ago", ago(note.at, board.now)),
                    look::META,
                    Weight::Regular,
                    ink.tertiary,
                ));
                let (word, color, about) = match delivery(note) {
                    Delivery::Held => ("held for review".to_string(), p.warning, String::new()),
                    Delivery::Denied => ("not delivered".into(), p.error, String::new()),
                    Delivery::Finished(to) => {
                        ("not delivered".into(), p.error, format!("{} had finished", to.join(", ")))
                    }
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
        // Who it did not reach, as the sender was told.
        for line in note.not_delivered() {
            ui.label(look::text(line, look::META, Weight::Regular, p.error));
        }
        // A message worth every agent's time becomes a pinned fact, once.
        let pinned = board.plan.notebook.iter().any(|fact| fact.pinned_from == Some(note.id) && fact.pinned);
        let pin = Command::CurateNote { plan: index, id: note.id, pinned: Some(true), delete: false };
        if !note.hidden && !pinned && board.plan.is_open() && backend.available(&pin) {
            ui.add_space(4.0);
            let response = small(ui, "Pin as fact", false, &p);
            flow::record(ui, &format!("button:Pin as fact {}", note.id), response.rect);
            if response.on_hover_text("Every agent of the board reads it, as a pinned fact").clicked() {
                client.send(pin);
            }
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
    fold
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
            undelivered: Default::default(),
            pinned_from: None,
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

    fn shape<'a>(threads: &'a [Thread<'a>]) -> Vec<(&'a str, Vec<(u64, usize)>)> {
        threads.iter().map(|t| (t.key.as_str(), t.messages.iter().map(|(n, d)| (n.id, *d)).collect())).collect()
    }

    #[test]
    fn threads_hold_both_directions_with_replies_under_what_they_answer() {
        let reply = |id, from: &str, to: &str, parent, at| NotebookEntry {
            reply_to: Some(parent),
            ..message(id, from, Some(to), "reply", at)
        };
        let notebook = vec![
            message(1, "regression", Some("command"), "Which exit code for a bad unit?", 10),
            message(2, "command", Some("regression"), "Exit 2, one line on stderr.", 20),
            message(3, "scout", Some("*"), "The fixture folder moved to tests/data.", 15),
            message(4, "scout", None, "CI caches target/; clear it after a toolchain bump.", 40),
            reply(5, "regression", "command", 2, 30),
            message(6, "regression", Some("scout"), "Thanks.", 25),
            // An answer to a broadcast from someone else sits under the broadcast.
            NotebookEntry { reply_to: Some(3), ..message(7, "command", Some("*"), "Seen.", 16) },
        ];
        let threads = threads(&notebook);
        assert_eq!(
            shape(&threads),
            [
                ("scout→all", vec![(3, 0), (7, 1), (4, 0)]),
                ("command⇄regression", vec![(1, 0), (2, 0), (5, 1)]),
                ("regression⇄scout", vec![(6, 0)]),
            ],
            "newest activity first; both directions together; broadcasts apart"
        );
        assert_eq!(threads[1].with.as_deref(), Some("regression"));
        assert_eq!((threads[0].from.as_str(), threads[0].with.as_deref()), ("scout", None));
        assert_eq!(threads[0].latest, 40);
    }

    #[test]
    fn decisions_and_what_waits_start_open_and_the_rest_folded() {
        let mut unread = message(2, "b", Some("a"), "Did you push?", 20);
        unread.pending = vec!["a".into()];
        let notebook = vec![
            message(1, "a", Some("*"), "API decided: convert(value, from, to).", 10),
            unread,
            message(3, "a", Some("c"), "ok", 30),
        ];
        let threads = threads(&notebook);
        let open: Vec<(&str, bool)> = threads.iter().map(|t| (t.key.as_str(), t.starts_open())).collect();
        assert_eq!(open, [("a⇄c", false), ("a⇄b", true), ("a→all", true)]);
        assert!(starts_open(&notebook[0]) && starts_open(&notebook[1]) && !starts_open(&notebook[2]));
        assert_eq!((threads[2].decisions(), threads[1].unread(), threads[0].held()), (1, 1, 0));
        // What someone opened or folded wins over how it starts.
        let mut folds = Folds::default();
        assert!(!folds.thread_open(&threads[0]) && folds.message_open(&notebook[0]));
        folds.threads.insert("a⇄c".into(), true);
        folds.messages.insert(1, false);
        assert!(folds.thread_open(&threads[0]) && !folds.message_open(&notebook[0]));
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
