//! A board's History: the Work tab's other face beside Live, the board as a run log ([`crate::run_log`]). A line
//! sums it up; who talks to whom shows how much passed each way between every two that talked, beside the board's
//! coordination settings that tune it; and the timeline lists what passed, newest first, each entry opening to its
//! whole text. Only the entries on screen are laid out, and nothing here moves, so a window at rest stays at rest.

use std::collections::HashSet;
use std::sync::Arc;

use atomics_core::config::CoordinationMode;
use atomics_core::notebook::{Edit, RelayDecision, RelayState};
use atomics_core::trace::{TracePhase, TraceState};
use atomics_core::trace_feed::TraceData;
use egui::{Align2, Color32, Rect, Sense, Stroke, StrokeKind, pos2, vec2};

use super::look::{self, Ink, Weight};
use crate::flow::{self, font};
use crate::run_log::{self, Entry, Log, Pair, Party, Relay, What};
use crate::theme::{Palette, lerp};
use crate::{Backend, BoardView, Client, Command};
use atomics_core::plan::Verdict;

/// What one client holds of a board's History.
#[derive(Debug, Clone, Default)]
pub struct View {
    /// The timeline's entries open to their whole text, by key.
    pub open: HashSet<String>,
    /// The two whose exchanges alone the timeline lists.
    pub pair: Option<(Party, Party)>,
}

/// The switch's width and height.
pub const SWITCH: egui::Vec2 = vec2(150.0, 28.0);
/// Below this width the two cards stack, and a timeline entry takes two lines.
const WIDE: f32 = 760.0;
const ONE_LINE: f32 = 560.0;

/// The Work tab's two faces, as a small segmented switch: Live, the flow; History, its run log.
pub fn switch(ui: &mut egui::Ui, client: &mut Client) {
    let p = client.palette.clone();
    let (rect, _) = ui.allocate_exact_size(SWITCH, Sense::hover());
    let painter = ui.painter().clone();
    painter.rect(
        rect,
        rect.height() / 2.0,
        lerp(p.background, p.raised, 0.6),
        Stroke::new(1.0, p.border),
        StrokeKind::Inside,
    );
    let w = (rect.width() - 6.0) / 2.0;
    for (k, (label, history)) in [("Live", false), ("History", true)].into_iter().enumerate() {
        let cell =
            Rect::from_min_size(pos2(rect.left() + 3.0 + w * k as f32, rect.top() + 3.0), vec2(w, rect.height() - 6.0));
        let response = ui.interact(cell, ui.scope_id().with(("work-face", label)), Sense::click());
        flow::record(ui, &format!("face:{label}"), cell);
        let on = client.simple.history == history;
        if on {
            painter.rect(
                cell,
                cell.height() / 2.0,
                lerp(p.raised, p.accent, 0.16),
                Stroke::new(1.0, lerp(p.border, p.accent, 0.5)),
                StrokeKind::Inside,
            );
        }
        crate::controls::feedback(ui, cell, &response, cell.height() / 2.0, &p);
        let hover = crate::controls::hover_amount(ui, response.id, response.hovered());
        let color = if on { p.foreground } else { lerp(Ink::of(&p).secondary, p.foreground, hover) };
        look::paint(&painter, cell.center(), Align2::CENTER_CENTER, label, look::META, Weight::Medium, color);
        let response = response.on_hover_text(if history {
            "What passed between the chief and its agents, newest first"
        } else {
            "The board's work as it happens"
        });
        if response.clicked() {
            client.simple.history = history;
        }
    }
}

/// The run log of `board`, made again only when what it is made from changed: the chief's calls the host keeps for
/// the board, then the live trace's.
fn log_of(ctx: &egui::Context, backend: &dyn Backend, board: &BoardView, trace: &TraceData) -> Arc<Log> {
    let plan = &board.plan;
    let kept = backend.past_calls(&plan.id).unwrap_or_default();
    let tasks: Vec<_> = plan
        .tasks
        .iter()
        .map(|t| {
            let round = t.reviews.last().map(|r| (r.round, r.closed, r.verdict));
            (t.dispatched_at, t.report.as_ref().map(|r| r.at), t.notes.len(), t.session.as_deref(), round)
        })
        .collect();
    let notes: Vec<_> = plan
        .notebook
        .iter()
        .map(|n| (n.id, n.pinned, n.hidden, n.relay.as_ref().map(|r| (r.state as u8, r.decided_at))))
        .collect();
    let calls: Vec<_> = trace
        .entries
        .iter()
        .map(|e| (e.seq, e.at, e.phase == TracePhase::Post, e.state == TraceState::Error))
        .collect();
    let of = (plan.id.as_str(), tasks, notes, calls, kept.len(), kept.last().map(|e| (e.seq, e.at)));
    crate::frame::kept(ctx, backend, "simple-run-log", of, || {
        Arc::new(run_log::build(plan, kept.iter().chain(&trace.entries)))
    })
}

/// The board's History, at the width the ui has.
pub fn show(ui: &mut egui::Ui, index: usize, board: &BoardView, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let ink = Ink::of(&p);
    let trace = backend.shared_trace();
    let log = log_of(ui.ctx(), backend, board, &trace);
    let sessions = backend.sessions();
    let subagents = run_log::subagents(&board.plan, |id| {
        sessions.iter().find(|s| s.id == id && board.plan.is_open()).and_then(|s| s.activity.as_ref())
    });
    ui.add_space(4.0);
    let line = ui.label(look::text(log.summary.line(), look::LABEL, Weight::Regular, ink.primary));
    flow::record(ui, "history:summary", line.rect);
    line.on_hover_text(
        "From the chief's calls the host keeps for this board, each task's latest report, and the board's notebook. \
         A task keeps only its latest report.",
    );
    ui.add_space(12.0);
    let width = ui.available_width();
    if width >= WIDE {
        let gap = 14.0;
        let left = ((width - gap) * 0.58).floor();
        let spacing = ui.spacing().item_spacing;
        ui.horizontal_top(|ui| {
            // No gap of its own between the columns; each keeps the spacing inside it.
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.allocate_ui_with_layout(vec2(left, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.spacing_mut().item_spacing = spacing;
                ui.set_width(left);
                talk(ui, &log, board, &subagents, client);
            });
            ui.add_space(gap);
            let right = ui.available_width();
            ui.allocate_ui_with_layout(vec2(right, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.spacing_mut().item_spacing = spacing;
                ui.set_width(right);
                reviews(ui, &log, board, client);
                ui.add_space(12.0);
                coordination(ui, index, board, backend, client);
            });
        });
    } else {
        talk(ui, &log, board, &subagents, client);
        ui.add_space(12.0);
        reviews(ui, &log, board, client);
        ui.add_space(12.0);
        coordination(ui, index, board, backend, client);
    }
    ui.add_space(18.0);
    timeline(ui, index, &log, board, backend, client);
}

/// A party's name as the History says it.
fn name(party: &Party) -> &str {
    party.short()
}

/// The title of the task a party is, for a pair's row.
fn title<'a>(party: &Party, board: &'a BoardView) -> Option<&'a str> {
    let Party::Task(id) = party else { return None };
    board.plan.tasks.iter().find(|t| &t.id == id).map(|t| t.title.as_str())
}

/// How much passed between each two that talked, the most first, as a bar each: out from the first named, then
/// back. A click lists only their exchanges in the timeline.
fn talk(ui: &mut egui::Ui, log: &Log, board: &BoardView, subagents: &[run_log::Subagents], client: &mut Client) {
    let p = client.palette.clone();
    let ink = Ink::of(&p);
    look::card(&p).inner_margin(egui::Margin::symmetric(16, 14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            look::section(ui, &p, "Who talks to whom");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let back = look::text("← back", look::SMALL, Weight::Medium, lerp(p.info, ink.secondary, 0.25));
                ui.label(back);
                ui.label(look::text("→ out", look::SMALL, Weight::Medium, lerp(p.accent, ink.secondary, 0.25)));
            });
        });
        ui.add_space(6.0);
        if log.pairs.is_empty() {
            ui.label(look::text("No one has talked on this board yet.", look::META, Weight::Regular, ink.secondary));
        }
        let most = log.pairs.iter().map(Pair::total).max().unwrap_or(1).max(1);
        for pair in &log.pairs {
            pair_row(ui, pair, most, board, client);
        }
        if !subagents.is_empty() {
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(2.0);
            for agents in subagents {
                let kinds: Vec<String> = agents
                    .kinds
                    .iter()
                    .map(|(kind, n)| if *n == 1 { kind.clone() } else { format!("{kind} ×{n}") })
                    .collect();
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(look::text(&agents.task, look::META, Weight::Medium, ink.primary));
                    let total = agents.total();
                    let words = format!("{total} subagent{}", if total == 1 { "" } else { "s" });
                    ui.label(look::text(words, look::META, Weight::Regular, ink.secondary));
                    ui.label(look::text(kinds.join(", "), look::META, Weight::Regular, ink.tertiary));
                    if agents.running > 0 {
                        ui.label(look::text(
                            format!("{} running", agents.running),
                            look::META,
                            Weight::Regular,
                            p.accent,
                        ));
                    }
                })
                .response
                .on_hover_text("Subagents the task's agent started: names and counts only, without their times.");
            }
        }
    });
}

/// One pair: their names, a bar as long as what passed between them beside the busiest pair's, and the counts.
fn pair_row(ui: &mut egui::Ui, pair: &Pair, most: usize, board: &BoardView, client: &mut Client) {
    let p = client.palette.clone();
    let ink = Ink::of(&p);
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
    let key = format!("pair:{}:{}", name(&pair.a), name(&pair.b));
    flow::record(ui, &key, rect);
    let picked = client.simple.run.pair.as_ref() == Some(&(pair.a.clone(), pair.b.clone()));
    let painter = ui.painter_at(rect.expand(2.0));
    if picked {
        painter.rect_filled(rect, 6.0, lerp(p.raised, p.accent, 0.12));
    }
    crate::controls::feedback(ui, rect, &response, 6.0, &p);
    let y = rect.center().y;
    let names = (rect.width() * 0.46).clamp(120.0, 300.0);
    let mut x = rect.left() + 6.0;
    let a =
        look::paint(&painter, pos2(x, y), Align2::LEFT_CENTER, name(&pair.a), look::LABEL, Weight::Medium, ink.primary);
    x = a.right() + 6.0;
    two_way(&painter, pos2(x + 6.0, y), ink.tertiary);
    x += 18.0;
    let b =
        look::paint(&painter, pos2(x, y), Align2::LEFT_CENTER, name(&pair.b), look::LABEL, Weight::Medium, ink.primary);
    x = b.right() + 8.0;
    let about = title(&pair.b, board).or_else(|| title(&pair.a, board)).unwrap_or_default();
    let room = rect.left() + names - x;
    if !about.is_empty() && room > 24.0 {
        let galley = flow::truncated(&painter, about, font(look::META), ink.secondary, room);
        painter.galley(pos2(x, y - galley.size().y / 2.0), galley, ink.secondary);
    }
    // The counts, at the right: out, then back.
    let out = lerp(p.accent, ink.primary, 0.15);
    let back = lerp(p.info, ink.primary, 0.15);
    let right = rect.right() - 6.0;
    let shown = look::paint(
        &painter,
        pos2(right, y),
        Align2::RIGHT_CENTER,
        &format!("← {}", pair.ba.total),
        look::META,
        Weight::Medium,
        if pair.ba.total > 0 { back } else { ink.tertiary },
    );
    let first = look::paint(
        &painter,
        pos2(shown.left() - 10.0, y),
        Align2::RIGHT_CENTER,
        &format!("→ {}", pair.ab.total),
        look::META,
        Weight::Medium,
        if pair.ab.total > 0 { out } else { ink.tertiary },
    );
    let bar = Rect::from_min_max(pos2(rect.left() + names + 8.0, y - 3.0), pos2(first.left() - 14.0, y + 3.0));
    if bar.width() > 16.0 {
        painter.rect_filled(bar, 3.0, lerp(p.background, p.border, 0.55));
        let unit = bar.width() / most as f32;
        let ab = Rect::from_min_size(bar.min, vec2(unit * pair.ab.total as f32, bar.height()));
        let ba = Rect::from_min_size(pos2(ab.right(), bar.top()), vec2(unit * pair.ba.total as f32, bar.height()));
        if pair.ab.total > 0 {
            painter.rect_filled(ab, 3.0, p.accent);
        }
        if pair.ba.total > 0 {
            painter.rect_filled(ba, 3.0, p.info);
        }
    }
    let response = response.on_hover_ui(|ui| {
        let line = |ui: &mut egui::Ui, from: &Party, to: &Party, tally: &run_log::Tally| {
            if tally.total > 0 {
                ui.label(format!("{} → {}: {}", name(from), name(to), tally.words()));
            }
        };
        line(ui, &pair.a, &pair.b, &pair.ab);
        line(ui, &pair.b, &pair.a, &pair.ba);
        ui.label(
            egui::RichText::new(if picked { "Click to list everything again" } else { "Click to list only these" })
                .color(ink.secondary),
        );
    });
    if response.clicked() {
        let picked = (pair.a.clone(), pair.b.clone());
        let run = &mut client.simple.run;
        run.pair = (run.pair.as_ref() != Some(&picked)).then_some(picked);
    }
}

/// The colour of a round's verdict: accepted or approved green, changes amber, blocked red; open, the accent.
fn verdict_tint(verdict: Option<Verdict>, p: &Palette) -> Color32 {
    match verdict {
        None => p.accent,
        Some(Verdict::Accepted | Verdict::Approve) => p.success,
        Some(Verdict::ChangesRequested | Verdict::Changes) => p.warning,
        Some(Verdict::Blocked) => p.error,
    }
}

/// How many rounds each task's review took, leading with that count, and its rounds as chips in order; the final
/// review last, the same way.
fn reviews(ui: &mut egui::Ui, log: &Log, board: &BoardView, client: &mut Client) {
    let p = client.palette.clone();
    let ink = Ink::of(&p);
    look::card(&p).inner_margin(egui::Margin::symmetric(16, 14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        look::section(ui, &p, "Reviews");
        ui.add_space(6.0);
        if log.reviews.is_empty() {
            let none = "No task has been in review yet.";
            ui.label(look::text(none, look::META, Weight::Regular, ink.secondary));
        }
        for task in &log.reviews {
            review_row(ui, task, board, &p);
        }
    });
}

/// One task's review: its name and where it stands, how long it took, and a chip per round.
fn review_row(ui: &mut egui::Ui, task: &run_log::Rounds, board: &BoardView, p: &Palette) {
    let ink = Ink::of(p);
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 46.0), Sense::hover());
    flow::record(ui, &format!("reviews:{}", task.task), rect);
    let painter = ui.painter_at(rect.expand(2.0));
    let top = rect.top() + 12.0;
    let name = look::paint(
        &painter,
        pos2(rect.left() + 2.0, top),
        Align2::LEFT_CENTER,
        task.name(),
        look::LABEL,
        Weight::Medium,
        ink.primary,
    );
    let last = task.rounds.last().and_then(|round| round.verdict);
    let open = task.rounds.last().is_some_and(|round| round.closed.is_none());
    let color = if open { ink.primary } else { lerp(verdict_tint(last, p), ink.primary, 0.2) };
    let words = look::paint(
        &painter,
        pos2(name.right() + 10.0, top),
        Align2::LEFT_CENTER,
        &task.words(),
        look::LABEL,
        Weight::Regular,
        color,
    );
    if let Some(took) = task.took() {
        let took = format!("in {}", run_log::duration(took));
        if words.right() + 80.0 < rect.right() {
            look::paint(
                &painter,
                pos2(rect.right() - 2.0, top),
                Align2::RIGHT_CENTER,
                &took,
                look::META,
                Weight::Regular,
                ink.tertiary,
            );
        }
    }
    // A chip per round, in order; one still open is an outline.
    let (w, h, gap) = (16.0, 8.0, 4.0);
    let mut x = rect.left() + 2.0;
    let y = rect.bottom() - 13.0;
    let mut hovered = None;
    for round in &task.rounds {
        if x + w > rect.right() {
            break;
        }
        let chip = Rect::from_min_size(pos2(x, y - h / 2.0), vec2(w, h));
        let tint = verdict_tint(round.verdict, p);
        if round.closed.is_none() {
            painter.rect(chip, 2.5, Color32::TRANSPARENT, Stroke::new(1.3, tint), StrokeKind::Inside);
        } else {
            painter.rect_filled(chip, 2.5, tint);
        }
        if ui.rect_contains_pointer(chip.expand2(vec2(gap / 2.0, 6.0))) {
            hovered = Some(round);
        }
        x += w + gap;
    }
    let title = board.plan.tasks.iter().find(|t| t.id == task.task).map(|t| t.title.as_str()).unwrap_or_default();
    response.on_hover_ui(|ui| {
        let Some(round) = hovered else {
            ui.label(egui::RichText::new(title).strong());
            ui.label(format!("{} review round{}", task.count(), if task.count() == 1 { "" } else { "s" }));
            return;
        };
        let verdict = run_log::verdict_words(round.verdict, round.blocking, task.final_review);
        ui.label(egui::RichText::new(format!("Round {} · {verdict}", round.round)).strong());
        let by = match round.by {
            Some(atomics_core::plan::Decider::Chief) => "by the chief",
            Some(atomics_core::plan::Decider::User) => "by you",
            Some(atomics_core::plan::Decider::Reviewer) => "by the reviewer",
            Some(atomics_core::plan::Decider::Agent) => "by its agent",
            None if round.closed.is_some() => "by the chief or you",
            None => "",
        };
        let long = round.closed.map(|closed| run_log::duration(closed.saturating_sub(round.opened)));
        let when = match long {
            Some(long) => format!("{by} after {long}"),
            None => format!("open for {}", run_log::duration(board.now.saturating_sub(round.opened))),
        };
        ui.label(egui::RichText::new(when.trim()).color(ink.secondary));
        if !round.said.is_empty() {
            ui.label(&round.said);
        } else if !round.report.is_empty() {
            ui.set_max_width(320.0);
            super::quoted(ui, &round.report, p);
        }
    });
}

/// A small two-way arrow, drawn: the faces Simple mode has need not have one.
fn two_way(painter: &egui::Painter, c: egui::Pos2, color: Color32) {
    let stroke = Stroke::new(1.2, color);
    for (dy, dir) in [(-2.5, 1.0), (2.5, -1.0)] {
        let (from, to) = (c + vec2(-5.0 * dir, dy), c + vec2(5.0 * dir, dy));
        painter.line_segment([from, to], stroke);
        painter.line_segment([to, to + vec2(-2.5 * dir, -2.0)], stroke);
        painter.line_segment([to, to + vec2(-2.5 * dir, 2.0)], stroke);
    }
}

/// The board's coordination settings as they stand, with the commands the full window's notebook sends.
fn coordination(ui: &mut egui::Ui, index: usize, board: &BoardView, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let ink = Ink::of(&p);
    let plan = &board.plan;
    look::card(&p).inner_margin(egui::Margin::symmetric(16, 14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        look::section(ui, &p, "Coordination");
        ui.add_space(6.0);
        let label_width = 112.0;
        let row = |ui: &mut egui::Ui, label: &str, add: &mut dyn FnMut(&mut egui::Ui)| {
            ui.horizontal_wrapped(|ui| {
                let (cell, _) = ui.allocate_exact_size(vec2(label_width, 26.0), Sense::hover());
                look::paint(ui.painter(), cell.left_center(), Align2::LEFT_CENTER, label, look::META, Weight::Regular, ink.secondary);
                add(ui);
            });
        };
        let mode = match (board.coordination_enabled, board.coordination_mode) {
            (false, _) | (_, Some(CoordinationMode::Off)) => None,
            (true, mode) => Some(mode),
        };
        let (word, about) = match mode {
            None => ("Off", "Agents cannot message each other on this board. The project's settings turn it on ([plan] coordination)."),
            Some(Some(CoordinationMode::Allow)) => ("Delivered at once", "An agent's message reaches the other without review ([plan] coordination = \"allow\")."),
            Some(Some(_)) => ("Held for review", "An agent's message to another waits until the chief or you deliver it ([plan] coordination = \"relay\")."),
            Some(None) => ("On", "Agents may message each other on this board."),
        };
        row(ui, "Agents' messages", &mut |ui| {
            let color = if mode.is_some() { ink.primary } else { ink.secondary };
            ui.label(look::text(word, look::LABEL, Weight::Medium, color)).on_hover_text(about);
        });
        if mode.is_none() {
            return;
        }
        let command = |edit| Command::Coordination { plan: index, edit };
        let can = backend.available(&command(Edit::Mute { task: None, muted: false })) && plan.is_open();
        let muted = plan.coordination.muted;
        row(ui, "This board", &mut |ui| {
            let (word, color) = if muted { ("Muted", p.warning) } else { ("Talking", p.success) };
            ui.label(look::text(word, look::LABEL, Weight::Medium, color));
            if can {
                let label = if muted { "Unmute" } else { "Mute" };
                let button = egui::Button::new(look::text(label, look::META, Weight::Regular, ink.primary)).min_size(vec2(0.0, 24.0));
                let response = ui.add(button);
                flow::record(ui, &format!("button:{label} board"), response.rect);
                if response.on_hover_text("Stop or let again every agent's messages on this board").clicked() {
                    client.send(command(Edit::Mute { task: None, muted: !muted }));
                }
            }
        });
        row(ui, "Tasks", &mut |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for task in plan.tasks.iter().filter(|t| t.id != atomics_core::plan::REVIEW_TASK) {
                let off = plan.coordination.muted_tasks.contains(&task.id);
                let color = if off { ink.tertiary } else { ink.primary };
                let mut text = look::text(&task.id, look::META, Weight::Medium, color);
                if off {
                    text = text.strikethrough();
                }
                let button = egui::Button::new(text).min_size(vec2(0.0, 24.0)).selected(!off && !muted);
                let response = ui.add_enabled(can, button);
                flow::record(ui, &format!("mute:{}", task.id), response.rect);
                let hint = format!("{} · {}", task.title, if off { "muted: click to let it talk" } else { "click to mute it" });
                if response.on_hover_text(hint).clicked() {
                    client.send(command(Edit::Mute { task: Some(task.id.clone()), muted: !off }));
                }
            }
        });
        let recent: usize = plan
            .coordination
            .rate
            .iter()
            .filter(|rate| board.now.saturating_sub(rate.at) < 60)
            .map(|rate| rate.deliveries)
            .sum();
        row(ui, "Last minute", &mut |ui| {
            let words = match recent {
                0 => "No deliveries".to_string(),
                1 => "1 delivery".to_string(),
                n => format!("{n} deliveries"),
            };
            ui.label(look::text(words, look::LABEL, Weight::Regular, ink.primary));
        });
        let held = plan
            .notebook
            .iter()
            .filter(|n| n.relay.as_ref().is_some_and(|r| r.state == RelayState::Pending))
            .count();
        if held > 0 {
            row(ui, "Waiting", &mut |ui| {
                let words = format!("{held} message{} for review", if held == 1 { "" } else { "s" });
                ui.label(look::text(words, look::LABEL, Weight::Medium, p.warning));
            });
        }
        if !plan.rules.text.trim().is_empty() {
            row(ui, "Rules", &mut |ui| {
                let first = plan.rules.text.lines().find(|l| !l.trim().is_empty()).unwrap_or_default();
                let response = ui.add(egui::Label::new(look::text(first, look::META, Weight::Regular, ink.secondary)).truncate());
                response.on_hover_text(plan.rules.text.clone());
            });
        }
    });
}

/// The colour of what passed, as the flow's envelopes have it.
pub fn tint(entry: &Entry, p: &Palette) -> Color32 {
    match &entry.what {
        What::Message | What::Dispatch => p.accent,
        What::Review | What::Note { .. } => p.info,
        What::Report(status) => match status.as_str() {
            "done" => p.success,
            "question" => p.warning,
            "blocked" => p.error,
            _ => p.info,
        },
        What::AgentMessage(Relay::Held) => p.warning,
        What::AgentMessage(Relay::Denied | Relay::Expired) => lerp(p.info, p.error, 0.5),
        What::AgentMessage(_) => lerp(p.info, p.accent, 0.35),
        What::Decided(Relay::Delivered, _) => p.success,
        What::Decided(Relay::Denied, _) => p.error,
        What::Decided(..) => p.quiet,
        What::Approval(decision) if decision == "deny" => p.error,
        What::Approval(_) => p.success,
        What::Update => p.muted,
        What::Round { verdict, .. } => verdict_tint(*verdict, p),
    }
}

/// How long ago, short: "12 s", "4 min", "2 h", "3 d".
pub fn ago(at: u64, now: u64) -> String {
    let secs = now.saturating_sub(at);
    match secs {
        0..60 => format!("{secs} s"),
        60..3600 => format!("{} min", secs / 60),
        3600..86400 => format!("{} h", secs / 3600),
        _ => format!("{} d", secs / 86400),
    }
}

/// What passed, newest first; what has no time last, under a word that says so.
fn timeline(ui: &mut egui::Ui, index: usize, log: &Log, board: &BoardView, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let ink = Ink::of(&p);
    ui.horizontal(|ui| {
        look::section(ui, &p, "Timeline");
        if let Some((a, b)) = client.simple.run.pair.clone() {
            ui.add_space(8.0);
            let text = look::text(format!("{} ⇄ {}   ✕", name(&a), name(&b)), look::META, Weight::Medium, ink.primary);
            let response = ui.add(egui::Button::new(text).min_size(vec2(0.0, 24.0)).selected(true));
            flow::record(ui, "history:all", response.rect);
            if response.on_hover_text("List everything again").clicked() {
                client.simple.run.pair = None;
            }
        }
    });
    ui.add_space(4.0);
    let pair = client.simple.run.pair.clone();
    let holds = |entry: &Entry| {
        pair.as_ref().is_none_or(|(a, b)| (entry.from == *a && entry.to == *b) || (entry.from == *b && entry.to == *a))
    };
    let mut any = false;
    let mut earlier = false;
    for entry in log.entries.iter().filter(|e| holds(e)) {
        any = true;
        if entry.at.is_none() && !earlier {
            earlier = true;
            ui.add_space(10.0);
            ui.label(look::text("Earlier · the host kept no time for these", look::META, Weight::Medium, ink.tertiary))
                .on_hover_text(
                    "Messages an agent was sent that the chief's trace no longer has: from the chief or from you.",
                );
            ui.add_space(2.0);
        }
        crate::frame::in_view(ui, ("run-entry", &board.plan.id, &entry.key), |ui| {
            row(ui, index, entry, board, backend, client);
        });
    }
    if !any {
        ui.label(look::text("Nothing yet.", look::META, Weight::Regular, ink.secondary));
    }
    ui.add_space(24.0);
}

/// One entry: when, from whom to whom with its envelope, what kind, and the first line of what it said. A click
/// opens it to the whole text.
fn row(ui: &mut egui::Ui, index: usize, entry: &Entry, board: &BoardView, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let ink = Ink::of(&p);
    let width = ui.available_width();
    let one_line = width >= ONE_LINE;
    let height = if one_line { 34.0 } else { 52.0 };
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::click());
    flow::record(ui, &format!("entry:{}", entry.key), rect);
    let open = client.simple.run.open.contains(&entry.key);
    let painter = ui.painter_at(rect.expand(2.0));
    if open {
        painter.rect_filled(rect, 6.0, lerp(p.background, p.raised, 0.9));
    }
    crate::controls::feedback(ui, rect, &response, 6.0, &p);
    let color = tint(entry, &p);
    let top = if one_line { rect.center().y } else { rect.top() + 15.0 };
    let mut x = rect.left() + 8.0;
    let when = entry.at.map_or("—".to_string(), |at| ago(at, board.now));
    look::paint(&painter, pos2(x + 44.0, top), Align2::RIGHT_CENTER, &when, look::META, Weight::Regular, ink.tertiary);
    x += 56.0;
    let from = look::paint(
        &painter,
        pos2(x, top),
        Align2::LEFT_CENTER,
        name(&entry.from),
        look::META,
        Weight::Medium,
        ink.primary,
    );
    flow::envelope(
        &painter,
        pos2(from.right() + 13.0, top),
        flow::MARK_ENVELOPE,
        color,
        lerp(color, p.background, 0.82),
    );
    let to = look::paint(
        &painter,
        pos2(from.right() + 26.0, top),
        Align2::LEFT_CENTER,
        name(&entry.to),
        look::META,
        Weight::Medium,
        ink.primary,
    );
    x = (x + 150.0).max(to.right() + 12.0);
    let word = entry.what.word(&entry.to);
    let kind = look::paint(
        &painter,
        pos2(x, top),
        Align2::LEFT_CENTER,
        &word,
        look::META,
        Weight::Medium,
        lerp(color, ink.primary, 0.25),
    );
    let first = entry.text.lines().find(|l| !l.trim().is_empty()).unwrap_or_default().trim();
    let (text_at, room) = if one_line {
        let left = (x + 128.0).max(kind.right() + 12.0);
        (pos2(left, top), rect.right() - 30.0 - left)
    } else {
        (pos2(rect.left() + 64.0, rect.bottom() - 15.0), rect.right() - 30.0 - rect.left() - 64.0)
    };
    if !first.is_empty() && room > 30.0 {
        let shade = if entry.agent { ink.secondary } else { ink.primary };
        let galley = flow::truncated(&painter, first, font(look::LABEL), shade, room);
        painter.galley(pos2(text_at.x, text_at.y - galley.size().y / 2.0), galley, shade);
    }
    // A chevron at the right says it opens.
    let c = pos2(rect.right() - 14.0, top);
    let chevron = if open {
        vec![c + vec2(-3.5, -1.5), c + vec2(0.0, 2.0), c + vec2(3.5, -1.5)]
    } else {
        vec![c + vec2(-1.5, -3.5), c + vec2(2.0, 0.0), c + vec2(-1.5, 3.5)]
    };
    painter.add(egui::Shape::line(chevron, Stroke::new(1.3, ink.tertiary)));
    // Hovering says how long ago more closely, where the row rounds it.
    let long = entry.at.map(|at| run_log::duration(board.now.saturating_sub(at))).filter(|long| *long != when);
    let response = match long {
        Some(long) => response.on_hover_text(format!("{long} ago")),
        None => response,
    };
    if response.clicked() {
        let run = &mut client.simple.run;
        if !run.open.remove(&entry.key) {
            run.open.insert(entry.key.clone());
        }
    }
    if open {
        details(ui, index, entry, board, backend, client);
    }
}

/// An open entry: its whole text, an agent's set apart as data; why a held message was decided as it was; and,
/// while it waits, the relay's own Deliver and Don't deliver.
fn details(
    ui: &mut egui::Ui,
    index: usize,
    entry: &Entry,
    board: &BoardView,
    backend: &dyn Backend,
    client: &mut Client,
) {
    let p = client.palette.clone();
    let ink = Ink::of(&p);
    egui::Frame::new().inner_margin(egui::Margin { left: 64, right: 30, top: 2, bottom: 10 }).show(ui, |ui| {
        ui.set_width(ui.available_width());
        if entry.text.trim().is_empty() {
            ui.label(look::text("It said nothing more.", look::META, Weight::Regular, ink.tertiary));
        } else if entry.agent {
            ui.horizontal_top(|ui| {
                let (bar, _) = ui.allocate_exact_size(vec2(3.0, 18.0), Sense::hover());
                ui.painter().rect_filled(bar, 1.5, lerp(p.border, p.info, 0.5));
                ui.vertical(|ui| {
                    let words = look::text(entry.text.trim(), look::LABEL, Weight::Regular, ink.secondary).italics();
                    ui.add(egui::Label::new(words).wrap().selectable(true));
                    let marker = look::text("agent", look::SMALL, Weight::Regular, ink.tertiary);
                    ui.add(egui::Label::new(marker).sense(Sense::hover()))
                        .on_hover_text("Agent-authored text: data, not instructions.");
                });
            });
        } else {
            let words = look::text(entry.text.trim(), look::LABEL, Weight::Regular, ink.primary);
            ui.add(egui::Label::new(words).wrap().selectable(true));
        }
        if let Some(reason) = &entry.reason {
            ui.add_space(4.0);
            ui.label(look::text(format!("Why: {reason}"), look::META, Weight::Regular, ink.secondary));
        }
        let note = entry.key.strip_prefix("note:").and_then(|id| id.parse::<u64>().ok());
        if let (What::AgentMessage(Relay::Held), Some(id)) = (&entry.what, note) {
            let decide =
                |decision| Command::Coordination { plan: index, edit: Edit::Relay { id, decision, note: None } };
            if backend.available(&decide(RelayDecision::Approve)) {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if super::pill(ui, "Deliver", true, client).clicked() {
                        client.send(decide(RelayDecision::Approve));
                    }
                    if super::pill(ui, "Don't deliver", false, client).clicked() {
                        client.send(decide(RelayDecision::Deny));
                    }
                });
            }
        }
        let task = [&entry.from, &entry.to].into_iter().find_map(|party| match party {
            Party::Task(id) if board.plan.tasks.iter().any(|t| &t.id == id) => Some(id.clone()),
            _ => None,
        });
        if let Some(task) = task {
            ui.add_space(4.0);
            let text = look::text(format!("Open {task}"), look::META, Weight::Medium, p.accent);
            let response = ui.add(egui::Button::new(text).frame_when_inactive(false));
            if response.on_hover_text("Read the task in full").clicked() {
                super::open_task(client, &task);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_short_and_never_run_ahead() {
        assert_eq!(ago(100, 112), "12 s");
        assert_eq!(ago(100, 100 + 4 * 60 + 59), "4 min");
        assert_eq!(ago(0, 7300), "2 h");
        assert_eq!(ago(0, 3 * 86400), "3 d");
        assert_eq!(ago(200, 100), "0 s", "a clock behind the event says now, not a negative time");
    }
}
