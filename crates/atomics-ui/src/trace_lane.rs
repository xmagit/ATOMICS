//! The trace lane under the chief's terminal: what the chief did with tickets, as cards
//! instead of JSON. Every call is one card (search, read, children, related, comment, link,
//! plan, or another MCP tool) with a spinner while it runs, then a check and its time, or a
//! cross and a short error. Cards come from `Runtime.traces`, already cleaned and bounded by
//! `atomics_core::trace`; the lane only draws them.
//!
//! Keys, with the lane focused (click it): Shift+↓/↑ step card by card, PageUp/PageDown
//! scroll, Enter expands a card or opens its ticket in the Tickets panel, Escape returns to the terminal.
use crate::theme::{Palette, lerp};
use crate::touch::Touch;
use crate::{Backend, Client, Command, panel_nav};
use atomics_core::tickets::TicketCard;
use atomics_core::trace::{TraceEntry, TraceOp, TraceState};
use atomics_core::trace_feed::TraceData;
use egui::text::{LayoutJob, TextWrapping};
use egui::{Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, pos2, vec2};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

const HEAD_H: f32 = 22.0;
const LINE_H: f32 = 18.0;
const CARD_PAD: f32 = 6.0;
/// Rows a floating card shows before "+ N more"; expanding shows all of them.
const SHOWN: usize = 3;
const MIN_H: f32 = 64.0;
const DEFAULT_H: f32 = 190.0;

/// The lane's keyboard target.
pub fn focus_id() -> egui::Id {
    egui::Id::NULL.with("trace-lane-keyboard")
}

#[derive(Default)]
pub struct Lane {
    pub collapsed: bool,
    /// Dragged height of the whole lane; the default when unset.
    pub height: Option<f32>,
    /// The selected card, by trace sequence number.
    pub selected: Option<u64>,
    pub expanded: HashSet<u64>,
    /// Cards up to this sequence number were cleared from view.
    pub cleared: u64,
    reveal: bool,
    content_h: f32,
    /// The lane had the keyboard focus when it was last drawn.
    pub had_focus: bool,
    session: Option<String>,
    /// A bounded ticket preview for clients without a Tickets panel.
    ticket: Option<TicketCard>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Glyph {
    Running,
    Ok,
    Error,
}

/// One line under a card's header.
#[derive(Clone, PartialEq, Debug)]
pub enum Line {
    /// A ticket of the entry's cards, by index.
    Ticket(usize),
    Text(String),
}

/// A trace entry as the lane shows it, before any painting.
#[derive(Clone, PartialEq, Debug)]
pub struct CardView {
    pub glyph: Glyph,
    pub kind: &'static str,
    pub title: String,
    pub detail: String,
    pub meta: String,
    pub lines: Vec<Line>,
    /// A ticket the card can open in the panel.
    pub open: Option<usize>,
}

fn arg(entry: &TraceEntry, names: &[&str]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(&entry.summary).ok()?;
    names.iter().find_map(|name| {
        value.get(*name).and_then(|v| v.as_str()).filter(|s| !s.is_empty() && !s.starts_with('[')).map(str::to_string)
    })
}

fn tickets_text(n: usize) -> String {
    format!("{n} ticket{}", if n == 1 { "" } else { "s" })
}

pub fn elapsed_text(ms: u64) -> String {
    format!("{:.1}s", ms as f64 / 1000.0)
}

/// What a trace entry looks like as a card, by kind (design §2.3, §5.4).
pub fn card_view(entry: &TraceEntry) -> CardView {
    let glyph = match entry.state {
        TraceState::Running => Glyph::Running,
        TraceState::Ok => Glyph::Ok,
        TraceState::Error => Glyph::Error,
    };
    let subject = || {
        arg(entry, &["ticket", "key", "id"])
            .or_else(|| entry.cards.first().map(|card| card.link.label().to_string()))
            .unwrap_or_default()
    };
    let count = entry.cards.len() + entry.omitted;
    let mut view = CardView {
        glyph,
        kind: "tool",
        title: String::new(),
        detail: String::new(),
        meta: String::new(),
        lines: Vec::new(),
        open: None,
    };
    let list = |view: &mut CardView| {
        view.lines = (0..entry.cards.len()).map(Line::Ticket).collect();
        if entry.omitted > 0 {
            view.lines.push(Line::Text(format!("+ {} more", entry.omitted)));
        }
        if glyph == Glyph::Ok {
            view.meta = tickets_text(count);
        }
        view.open = (!entry.cards.is_empty()).then_some(0);
    };
    match entry.op {
        TraceOp::Search | TraceOp::SemanticSearch => {
            view.kind = "search";
            view.title = if entry.op == TraceOp::SemanticSearch { "search by meaning" } else { "search" }.into();
            view.detail = arg(entry, &["query"]).map(|q| format!("“{q}”")).unwrap_or_default();
            list(&mut view);
        }
        TraceOp::Read => {
            view.kind = "read";
            view.title = format!("read {}", subject());
            if let Some(card) = entry.cards.first() {
                let facts = [card.link.state.as_ref().map(|s| format!("[{s}]")), card.kind.clone()];
                view.detail = facts.into_iter().flatten().collect::<Vec<_>>().join(" ");
                let chars = card.body.as_deref().map_or(0, |b| b.chars().count());
                view.lines.push(Line::Ticket(0));
                view.lines.push(Line::Text(format!(
                    "{chars} chars · {} comment{} · {} children",
                    card.comments.len(),
                    if card.comments.len() == 1 { "" } else { "s" },
                    card.children.len()
                )));
                view.open = Some(0);
            }
        }
        TraceOp::Children => {
            view.kind = "children";
            view.title = format!("children of {}", subject());
            list(&mut view);
        }
        TraceOp::Related => {
            view.kind = "related";
            view.title = format!("related to {}", subject());
            list(&mut view);
        }
        TraceOp::Comment => {
            view.kind = "comment";
            view.title = format!("comment on {}", subject());
        }
        TraceOp::LinkOrSync => {
            view.kind = "link";
            let verb = entry.tool.strip_prefix("atomics_ticket_").unwrap_or(&entry.tool);
            view.title = format!("ticket {verb} {}", subject()).trim_end().to_string();
            list(&mut view);
        }
        TraceOp::Plan => {
            view.kind = "plan";
            let plan = entry.plan.as_ref();
            let name = plan.and_then(|p| p.id.clone()).or_else(|| entry.plan_id.clone()).unwrap_or_default();
            let tasks = plan.map_or(0, |p| p.tasks.len()) + entry.omitted;
            let verb = entry.tool.strip_prefix("atomics_plan_").unwrap_or("set");
            view.title = if verb == "set" {
                format!("plan {name}").trim_end().to_string()
            } else {
                format!("plan {verb} {name}").trim_end().to_string()
            };
            view.meta =
                if tasks > 0 { format!("{tasks} task{}", if tasks == 1 { "" } else { "s" }) } else { String::new() };
            if let Some(plan) = plan {
                view.detail = plan.goal.clone();
                for task in &plan.tasks {
                    let mut line = format!("{} {}", task.id, task.title).trim().to_string();
                    if !task.depends_on.is_empty() {
                        line.push_str(&format!("  ← {}", task.depends_on.join(", ")));
                    }
                    if let Some(ticket) = &task.ticket {
                        line.push_str(&format!("  · {ticket}"));
                    }
                    view.lines.push(Line::Text(line));
                }
                if entry.omitted > 0 {
                    view.lines.push(Line::Text(format!("+ {} more", entry.omitted)));
                }
                let tracked = plan.tasks.iter().filter(|t| t.ticket.is_some()).count();
                if !plan.tasks.is_empty() {
                    view.lines
                        .push(Line::Text(format!("{tracked} tracked · {} untracked", plan.tasks.len() - tracked)));
                }
            }
        }
        TraceOp::Other => {
            view.title = format!("{}: {}", entry.server, entry.tool);
            view.detail = entry.summary.clone();
            if entry.result_bytes > 0 {
                view.meta = format!("{} bytes", entry.result_bytes);
            }
        }
    }
    if let Some(error) = &entry.error {
        view.meta = format!("error: {error}");
    } else if let Some(hint) = &entry.hint {
        view.lines.push(Line::Text(hint.clone()));
    }
    view
}

/// The lines a card shows: none when docked and collapsed, a few when floating, all when expanded.
pub fn shown_lines(view: &CardView, expanded: bool, docked: bool) -> Vec<Line> {
    if expanded || view.lines.len() <= SHOWN && !docked {
        return view.lines.clone();
    }
    if docked {
        return Vec::new();
    }
    let mut lines: Vec<Line> = view.lines.iter().take(SHOWN).cloned().collect();
    lines.push(Line::Text(format!("+ {} more", view.lines.len() - SHOWN)));
    lines
}

fn card_height(lines: usize) -> f32 {
    HEAD_H + lines as f32 * LINE_H + if lines > 0 { CARD_PAD } else { 0.0 }
}

impl Lane {
    /// Keep local navigation bounded as the host evicts old calls or changes chief.
    pub fn sync(&mut self, data: &TraceData) {
        if self.session.is_some() && self.session != data.session {
            *self = Self::default();
        }
        self.session = data.session.clone();
        self.expanded.retain(|seq| data.entries.iter().any(|entry| entry.seq == *seq));
        if self.selected.is_some_and(|seq| !data.entries.iter().any(|entry| entry.seq == seq)) {
            self.selected = None;
        }
    }

    pub fn entries<'a>(&self, data: &'a TraceData) -> Vec<&'a TraceEntry> {
        data.entries.iter().filter(|entry| entry.seq > self.cleared).collect()
    }

    pub fn step(&mut self, data: &TraceData, delta: i32) {
        let seqs: Vec<u64> = self.entries(data).iter().map(|entry| entry.seq).collect();
        let current = self.selected.and_then(|seq| seqs.iter().position(|s| *s == seq));
        self.selected = panel_nav::step_index(current, seqs.len(), delta).map(|i| seqs[i]);
        self.reveal = true;
    }

    pub fn echo(&self, data: &TraceData) -> Option<String> {
        let entry = *self.entries(data).last()?;
        let view = card_view(entry);
        let mark = match view.glyph {
            Glyph::Running => "…",
            Glyph::Ok => "✓",
            Glyph::Error => "✗",
        };
        Some(format!("chief · {} {mark}", view.title))
    }
}

/// Process navigation before the terminal, on every client.
pub fn shortcuts(ctx: &egui::Context, client: &mut Client, data: &TraceData) {
    client.lane.sync(data);
    let focused = ctx.memory(|m| m.has_focus(focus_id()));
    if (focused || client.lane.had_focus) && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
    {
        client.lane.had_focus = false;
        ctx.memory_mut(|m| m.surrender_focus(focus_id()));
        client.focus_terminal = data.session.clone();
        return;
    }
    if focused {
        for (key, delta) in [(egui::Key::ArrowDown, 1), (egui::Key::ArrowUp, -1)] {
            let chord = egui::KeyboardShortcut::new(egui::Modifiers::SHIFT, key);
            if ctx.input_mut(|i| crate::keys::consume_physical_shortcut(i, &chord)) {
                client.lane.step(data, delta);
            }
        }
    }
}

enum Act {
    Toggle(u64),
    Select(u64),
    Open(Box<TicketCard>),
    Clear,
    Collapse,
    Focus,
}

fn spinner(time: f64) -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[(time * 12.0) as usize % FRAMES.len()]
}

/// One ticket row inside a card: score bar, key, state, title.
fn ticket_row(
    ui: &egui::Ui,
    rect: Rect,
    card: &TicketCard,
    range: Option<(f64, f64)>,
    data: &TraceData,
    p: &Palette,
    hovered: bool,
) {
    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    if hovered {
        painter.rect_filled(rect, 2.0, p.raised);
    }
    let y = rect.top() + 2.0;
    let mut x = rect.left();
    if let (Some(score), Some((low, high))) = (card.score, range) {
        let bar = Rect::from_min_size(pos2(x, y + 4.0), vec2(40.0, 7.0));
        painter.rect_filled(bar, 2.0, p.border);
        let full = relative_score(score, low, high);
        painter.rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * full, bar.height())), 2.0, p.accent);
        let text = fit(ui, &format!("{score:.2}"), p.mono(10.5), p.muted, 36.0);
        let width = text.size().x;
        painter.galley(pos2(bar.right() + 4.0, y + 1.0), text, p.muted);
        x = bar.right() + 4.0 + width + 8.0;
    }
    let key = fit(ui, card.link.label(), p.mono(11.0), p.accent, rect.right() - x);
    let width = key.size().x;
    painter.galley(pos2(x, y), key, p.accent);
    x += width + 8.0;
    if let Some(state) = &card.link.state {
        let color = data.states.get(state).map_or(p.muted, |status| crate::status::task_color(*status, p));
        x = chip(ui, &painter, pos2(x, y), state, color, rect.right(), p);
    }
    if x < rect.right() - 12.0
        && let Some(title) = card.link.title.as_deref()
    {
        let galley = fit(ui, title, p.ui(11.0), p.foreground, rect.right() - x);
        painter.galley(pos2(x, y), galley, p.foreground);
    }
}

/// The lane's header, with room for its buttons: a finger's are taller than a mouse's.
fn head(touch: Option<Touch>) -> f32 {
    touch.map_or(HEAD_H, |touch| HEAD_H.max(touch.target + 4.0))
}

impl Lane {
    /// Height the lane takes in a column `available` tall, 0 when the chief has no trace in view. Folded, it is its
    /// header, which under `touch` holds a finger's buttons.
    pub fn height(&self, data: &TraceData, available: f32, touch: Option<Touch>) -> f32 {
        if self.entries(data).is_empty() {
            return 0.0;
        }
        if self.collapsed {
            return head(touch) + 4.0;
        }
        self.height.unwrap_or(DEFAULT_H).clamp(MIN_H, (available * 0.7).max(MIN_H))
    }
}

/// Draws the same trace lane on the desktop, connected native client and web.
pub fn show(ui: &mut egui::Ui, rect: Rect, docked: bool, backend: &dyn Backend, client: &mut Client, data: &TraceData) {
    let p = client.palette.clone();
    let head = head(client.touch);
    let now = backend.trace_now_ms();
    let time = ui.input(|i| i.time);
    let mut acts: Vec<Act> = Vec::new();
    ui.painter().rect_filled(rect, 0.0, lerp(p.terminal, p.raised, 0.25));
    ui.painter().line_segment([rect.left_top(), rect.right_top()], Stroke::new(1.0, p.border));
    let lane = &mut client.lane;
    let entries = lane.entries(data);
    let mut inner = ui.new_child(egui::UiBuilder::new().id_salt("trace-lane").max_rect(rect.shrink2(vec2(6.0, 2.0))));
    let ui = &mut inner;
    ui.set_clip_rect(ui.clip_rect().intersect(rect));
    let running = entries.iter().any(|entry| entry.state == TraceState::Running);
    let focused = ui.memory(|m| m.has_focus(focus_id()));
    lane.had_focus = focused;
    ui.horizontal(|ui| {
        ui.set_height(head - 2.0);
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
        let plan = entries.iter().rev().find_map(|entry| entry.plan_id.clone());
        let label = plan.map_or("TRACE".to_string(), |plan| format!("TRACE · plan {plan}"));
        let color = if focused { p.accent } else { p.muted };
        ui.label(RichText::new(label).size(10.5).strong().color(color));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .button(if lane.collapsed { "Show" } else { "Hide" })
                .on_hover_text("Show or hide the trace's calls")
                .clicked()
            {
                acts.push(Act::Collapse);
            }
            if ui.button("Clear").on_hover_text("Forget the calls shown here").clicked() {
                acts.push(Act::Clear);
            }
            let calls = entries.len();
            let total = entries.iter().filter_map(|entry| entry.elapsed_ms).fold(0u64, u64::saturating_add);
            ui.label(
                RichText::new(format!("{calls} call{} · {}", if calls == 1 { "" } else { "s" }, elapsed_text(total)))
                    .size(11.0)
                    .color(p.muted),
            );
        });
    });
    if !lane.collapsed {
        let body = Rect::from_min_max(pos2(rect.left(), rect.top() + head), rect.right_bottom());
        let focus = ui.interact(body, focus_id(), Sense::click());
        if focus.clicked() {
            acts.push(Act::Focus);
        }
        if focused {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    focus_id(),
                    egui::EventFilter { vertical_arrows: true, escape: true, ..Default::default() },
                );
            });
            for key in panel_nav::take_keys_for(ui, focus_id(), &[egui::Key::Enter]) {
                let _ = key;
                if let Some(seq) = lane.selected {
                    acts.push(Act::Toggle(seq));
                }
            }
        }
        let views: Vec<CardView> = entries.iter().map(|entry| card_view(entry)).collect();
        let shown: Vec<Vec<Line>> = views
            .iter()
            .zip(&entries)
            .map(|(view, entry)| shown_lines(view, lane.expanded.contains(&entry.seq), docked))
            .collect();
        let scroll_id = ui.make_persistent_id(egui::IdSalt::new("trace-lane-scroll"));
        let mut jump = None;
        if std::mem::take(&mut lane.reveal)
            && let Some(at) = entries.iter().position(|entry| Some(entry.seq) == lane.selected)
        {
            let top: f32 = shown[..at].iter().map(|lines| card_height(lines.len())).sum();
            let bottom = top + card_height(shown[at].len());
            let current = egui::scroll_area::State::load(ui.ctx(), scroll_id).map_or(0.0, |s| s.clamped_offset().y);
            let height = body.height() - HEAD_H;
            jump = Some(if top < current {
                top
            } else if bottom > current + height {
                bottom - height
            } else {
                current
            });
        }
        let rows = (lane.content_h / LINE_H).ceil() as usize;
        let jump =
            panel_nav::keyboard_scroll_for(ui, focus_id(), scroll_id, LINE_H, 0..rows, &panel_nav::SCROLL_KEYS, jump);
        let mut area = egui::ScrollArea::vertical()
            .id_salt("trace-lane-scroll")
            .auto_shrink([false, false])
            .stick_to_bottom(lane.selected.is_none());
        if let Some(offset) = jump {
            area = area.vertical_scroll_offset(offset);
        }
        ui.spacing_mut().item_spacing.y = 0.0;
        let output = area.show(ui, |ui| {
            for ((entry, view), lines) in entries.iter().zip(&views).zip(&shown) {
                let width = ui.available_width();
                let (card, response) = ui.allocate_exact_size(vec2(width, card_height(lines.len())), Sense::click());
                let painter = ui.painter().with_clip_rect(card.intersect(ui.clip_rect()));
                let selected = lane.selected == Some(entry.seq);
                if selected {
                    painter.rect_filled(card.shrink2(vec2(0.0, 1.0)), 3.0, lerp(p.raised, p.accent, 0.14));
                    painter.rect_filled(Rect::from_min_size(card.left_top(), vec2(3.0, card.height())), 0.0, p.accent);
                } else if response.hovered() {
                    painter.rect_filled(card.shrink2(vec2(0.0, 1.0)), 3.0, p.raised);
                }
                if response.clicked() {
                    acts.push(Act::Select(entry.seq));
                    acts.push(Act::Toggle(entry.seq));
                    acts.push(Act::Focus);
                }
                let left = card.left() + 8.0;
                let right = card.right() - 6.0;
                let y = card.top() + 3.0;
                let (mark, mark_color) = match view.glyph {
                    Glyph::Running => (spinner(time), p.accent),
                    Glyph::Ok => ("✓", p.success),
                    Glyph::Error => ("✗", p.error),
                };
                painter.text(pos2(left, y), egui::Align2::LEFT_TOP, mark, p.mono(12.0), mark_color);
                let elapsed = match (entry.elapsed_ms, view.glyph) {
                    (Some(ms), _) => elapsed_text(ms),
                    (None, Glyph::Running) => elapsed_text(now.saturating_sub(entry.at)),
                    (None, _) => "–".into(),
                };
                painter.text(pos2(left + 16.0, y + 1.0), egui::Align2::LEFT_TOP, &elapsed, p.mono(10.5), p.muted);
                let mut meta_left = right;
                if let Some(first) = view.open.and_then(|i| entry.cards.get(i)) {
                    let zone = Rect::from_min_size(pos2(right - 44.0, card.top()), vec2(44.0, HEAD_H));
                    let open = ui.interact(zone, egui::Id::NULL.with(("lane-open", entry.seq)), Sense::click());
                    painter.text(
                        zone.right_center() - vec2(2.0, 0.0),
                        egui::Align2::RIGHT_CENTER,
                        "open ↗",
                        p.ui(11.0),
                        p.accent,
                    );
                    if open.clicked() {
                        acts.push(Act::Open(Box::new(first.clone())));
                    }
                    meta_left = zone.left() - 6.0;
                }
                if !view.meta.is_empty() {
                    let color = if view.glyph == Glyph::Error { p.error } else { p.muted };
                    let galley = fit(ui, &view.meta, p.ui(11.0), color, (meta_left - left - 120.0).max(60.0));
                    meta_left -= galley.size().x;
                    painter.galley(pos2(meta_left, y + 1.0), galley, color);
                    meta_left -= 8.0;
                }
                let x = left + 16.0 + 38.0;
                let title = fit(ui, &view.title, p.ui(12.0), p.foreground, meta_left - x);
                let title_w = title.size().x;
                painter.galley(pos2(x, y), title, p.foreground);
                if !view.detail.is_empty() && meta_left - x - title_w > 30.0 {
                    let galley = fit(ui, &view.detail, p.ui(11.0), p.muted, meta_left - x - title_w - 10.0);
                    painter.galley(pos2(x + title_w + 8.0, y + 0.5), galley, p.muted);
                }
                let range = score_range(&entry.cards);
                for (n, line) in lines.iter().enumerate() {
                    let row = Rect::from_min_size(
                        pos2(left + 16.0 + 38.0, card.top() + HEAD_H + n as f32 * LINE_H),
                        vec2((right - x).max(1.0), LINE_H),
                    );
                    match line {
                        Line::Ticket(i) => {
                            let Some(ticket) = entry.cards.get(*i) else { continue };
                            let hit =
                                ui.interact(row, egui::Id::NULL.with(("lane-row", entry.seq, *i)), Sense::click());
                            ticket_row(ui, row, ticket, range, data, &p, hit.hovered());
                            if hit.clicked() {
                                acts.push(Act::Open(Box::new(ticket.clone())));
                            }
                            hit.on_hover_ui(|ui| {
                                let link = &ticket.link;
                                let mut facts = vec![link.system.clone()];
                                facts.extend(link.updated_at.as_ref().map(|t| format!("updated {t}")));
                                facts.extend(link.synced_at.map(|t| format!("synced {t}")));
                                ui.label(facts.join(" · "));
                            });
                        }
                        Line::Text(text) => {
                            let color = if entry.state == TraceState::Error { p.error } else { p.muted };
                            let galley = fit(ui, text, p.ui(11.0), color, row.width());
                            painter.galley(row.min + vec2(0.0, 2.0), galley, color);
                        }
                    }
                }
            }
        });
        lane.content_h = output.content_size.y;
    }
    if running {
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }
    for act in acts {
        match act {
            Act::Toggle(seq) => {
                if !lane.expanded.remove(&seq) {
                    lane.expanded.insert(seq);
                }
            }
            Act::Select(seq) => lane.selected = Some(seq),
            Act::Open(card) => {
                if backend.available(&Command::OpenTicket(card.link.clone())) {
                    client.commands.push(Command::OpenTicket(card.link.clone()));
                } else {
                    lane.ticket = Some(*card);
                }
            }
            Act::Clear => {
                lane.cleared = data.entries.last().map_or(0, |entry| entry.seq);
                lane.selected = None;
            }
            Act::Collapse => lane.collapsed = !lane.collapsed,
            Act::Focus => ui.memory_mut(|m| m.request_focus(focus_id())),
        }
    }
    ticket_preview(ui.ctx(), lane);
}

/// Clients without a ticket reader can still open the cleaned card the trace carries.
fn ticket_preview(ctx: &egui::Context, lane: &mut Lane) {
    let Some(card) = lane.ticket.as_ref() else { return };
    let mut open = true;
    egui::Window::new(card.link.label())
        .id(egui::Id::NULL.with("trace-ticket"))
        .open(&mut open)
        .default_width(480.0)
        .max_width((ctx.content_rect().width() - 16.0).max(1.0))
        .max_height(ctx.content_rect().height() * 0.8)
        .show(ctx, |ui| {
            if let Some(title) = &card.link.title {
                ui.heading(title);
            }
            if let Some(state) = &card.link.state {
                ui.label(state);
            }
            if let Some(url) = card.link.url.as_deref().and_then(crate::markdown::web_url) {
                ui.hyperlink_to("Open in browser ↗", url);
            }
            egui::ScrollArea::vertical().show(ui, |ui| {
                if let Some(body) = &card.body {
                    ui.label(body);
                }
                for comment in &card.comments {
                    ui.separator();
                    ui.label(&comment.text);
                }
            });
        });
    if !open {
        lane.ticket = None;
    }
}

/// The handle above the lane: drag to resize, double-click to reset. Returns the new height.
pub fn lane_divider(ui: &mut egui::Ui, rect: Rect, height: &mut Option<f32>, p: &Palette) {
    let handle = Rect::from_center_size(rect.center_top(), vec2(rect.width(), 8.0));
    let response = ui.interact(handle, egui::Id::NULL.with("trace-lane-divider"), Sense::click_and_drag());
    if response.hovered() || response.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
        ui.painter().rect_filled(handle.shrink2(vec2(0.0, 3.0)), 0.0, p.accent.gamma_multiply(0.6));
    }
    if let (true, Some(pointer)) = (response.dragged() || response.drag_stopped(), response.interact_pointer_pos()) {
        *height = Some((rect.bottom() - pointer.y).max(MIN_H));
    }
    if response.double_clicked() {
        *height = None;
    }
}

/// How full a score's bar is, relative to the lowest and highest score of the set. One
/// result, or equal scores, fill the bar. The lowest keeps a visible stub.
pub fn relative_score(score: f64, low: f64, high: f64) -> f32 {
    if high <= low { 1.0 } else { (0.1 + 0.9 * (score - low) / (high - low)).clamp(0.0, 1.0) as f32 }
}

pub fn score_range(cards: &[TicketCard]) -> Option<(f64, f64)> {
    let mut scores = cards.iter().filter_map(|c| c.score).filter(|s| s.is_finite());
    let first = scores.next()?;
    Some(scores.fold((first, first), |(low, high), s| (low.min(s), high.max(s))))
}

pub fn fit(ui: &egui::Ui, text: &str, font: FontId, color: Color32, width: f32) -> Arc<egui::Galley> {
    let mut job = LayoutJob::simple_singleline(text.replace('\n', " "), font, color);
    job.wrap =
        TextWrapping { max_width: width.max(1.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    ui.painter().layout_job(job)
}

/// Paints a state chip at `at` and returns the x where the next thing may start.
pub fn chip(
    ui: &egui::Ui,
    painter: &egui::Painter,
    at: Pos2,
    text: &str,
    color: Color32,
    limit: f32,
    p: &Palette,
) -> f32 {
    let galley = fit(ui, text, p.ui(11.0), color, limit - at.x - 10.0);
    let rect = Rect::from_min_size(at, galley.size() + vec2(10.0, 2.0));
    painter.rect_filled(rect, 8.0, lerp(p.raised, color, 0.12));
    painter.rect_stroke(rect, 8.0, Stroke::new(1.0, color.gamma_multiply(0.6)), egui::StrokeKind::Inside);
    painter.galley(at + vec2(5.0, 1.0), galley, color);
    rect.right() + 8.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomics_core::tickets::TicketLink;
    use atomics_core::trace::{TracePhase, TracePlan, TraceTask};
    fn entry(op: TraceOp, state: TraceState) -> TraceEntry {
        TraceEntry {
            seq: 1,
            at: 0,
            phase: TracePhase::Post,
            tool_use_id: None,
            server: "atomics".into(),
            tool: "atomics_ticket_search".into(),
            op,
            direct: false,
            plan_id: None,
            summary: r#"{"query":"sign in"}"#.into(),
            state,
            elapsed_ms: Some(400),
            cards: Vec::new(),
            plan: None,
            result_bytes: 0,
            error: None,
            hint: None,
            truncated: false,
            omitted: 0,
        }
    }

    fn ticket(number: u32) -> TicketCard {
        TicketCard {
            link: TicketLink {
                system: "demo".into(),
                id: format!("id-{number}"),
                key: Some(format!("DEMO-{number}")),
                title: Some(format!("Ticket {number}")),
                state: Some("Backlog".into()),
                ..Default::default()
            },
            score: Some(0.9 - number as f64 / 100.0),
            ..Default::default()
        }
    }

    #[test]
    fn trace_lane_cards_say_what_each_kind_did() {
        let mut search = entry(TraceOp::SemanticSearch, TraceState::Ok);
        search.cards = (1..=5).map(ticket).collect();
        let view = card_view(&search);
        assert_eq!((view.kind, view.glyph), ("search", Glyph::Ok));
        assert_eq!((view.title.as_str(), view.detail.as_str()), ("search by meaning", "“sign in”"));
        assert_eq!(view.meta, "5 tickets");
        assert_eq!(view.open, Some(0));

        let mut read = entry(TraceOp::Read, TraceState::Ok);
        read.tool = "atomics_ticket_read".into();
        read.summary = r#"{"ticket":"DEMO-1"}"#.into();
        read.cards = vec![TicketCard { body: Some("12345".into()), ..ticket(1) }];
        let view = card_view(&read);
        assert_eq!(view.title, "read DEMO-1");
        assert_eq!(view.detail, "[Backlog]");
        assert_eq!(view.lines[1], Line::Text("5 chars · 0 comments · 0 children".into()));

        let mut children = entry(TraceOp::Children, TraceState::Ok);
        children.summary = r#"{"ticket":"DEMO-1"}"#.into();
        children.cards = (2..=3).map(ticket).collect();
        children.omitted = 4;
        let view = card_view(&children);
        assert_eq!(view.title, "children of DEMO-1");
        assert_eq!(view.meta, "6 tickets");
        assert_eq!(view.lines.last(), Some(&Line::Text("+ 4 more".into())));

        let mut related = entry(TraceOp::Related, TraceState::Running);
        related.summary = r#"{"ticket":"DEMO-1"}"#.into();
        let view = card_view(&related);
        assert_eq!((view.title.as_str(), view.glyph, view.meta.as_str()), ("related to DEMO-1", Glyph::Running, ""));

        let mut plan = entry(TraceOp::Plan, TraceState::Running);
        plan.tool = "atomics_plan_set".into();
        plan.plan = Some(TracePlan {
            id: Some("demo-1".into()),
            goal: "Goal".into(),
            tasks: vec![
                TraceTask { id: "t1".into(), title: "First".into(), depends_on: vec![], ticket: Some("DEMO-2".into()) },
                TraceTask { id: "t2".into(), title: "Second".into(), depends_on: vec!["t1".into()], ticket: None },
            ],
        });
        let view = card_view(&plan);
        assert_eq!((view.title.as_str(), view.meta.as_str()), ("plan demo-1", "2 tasks"));
        assert_eq!(view.lines[0], Line::Text("t1 First  · DEMO-2".into()));
        assert_eq!(view.lines[1], Line::Text("t2 Second  ← t1".into()));
        assert_eq!(view.lines[2], Line::Text("1 tracked · 1 untracked".into()));

        let mut failed = entry(TraceOp::Other, TraceState::Error);
        failed.server = "other".into();
        failed.tool = "lookup".into();
        failed.error = Some("unauthorized".into());
        let view = card_view(&failed);
        assert_eq!(
            (view.glyph, view.title.as_str(), view.meta.as_str()),
            (Glyph::Error, "other: lookup", "error: unauthorized")
        );
    }

    #[test]
    fn trace_lane_collapses_in_the_docked_column_and_shows_a_few_rows_floating() {
        let mut search = entry(TraceOp::Search, TraceState::Ok);
        search.cards = (1..=5).map(ticket).collect();
        let view = card_view(&search);
        assert!(shown_lines(&view, false, true).is_empty());
        let floating = shown_lines(&view, false, false);
        assert_eq!(floating.len(), SHOWN + 1);
        assert_eq!(floating.last(), Some(&Line::Text("+ 2 more".into())));
        assert_eq!(shown_lines(&view, true, true).len(), 5);
        assert_eq!(shown_lines(&view, true, false), view.lines);
    }
}
