//! The trace lane under the chief's terminal: what the chief did with tickets, as cards
//! instead of JSON. Every call is one card (search, read, children, related, comment, link,
//! plan, or another MCP tool) with a spinner while it runs, then a check and its time, or a
//! cross and a short error. Cards come from `Runtime.traces`, already cleaned and bounded by
//! `atomics_core::trace`; the lane only draws them.
//!
//! Keys, with the lane focused (click it): Shift+↓/↑ step card by card, PageUp/PageDown
//! scroll, Enter expands a card or opens its ticket in the Tickets panel, Escape returns to the terminal.
use super::tickets_panel::{chip, fit, relative_score, score_range, state_color};
use super::*;
use crate::panel_nav;
use atomics_core::tickets::{TicketCard, TicketConfig, TicketLink};
use atomics_core::trace::{TraceEntry, TraceOp, TraceState};

const HEAD_H: f32 = 22.0;
const LINE_H: f32 = 18.0;
const CARD_PAD: f32 = 6.0;
/// Rows a floating card shows before "+ N more"; expanding shows all of them.
const SHOWN: usize = 3;
const MIN_H: f32 = 64.0;
const DEFAULT_H: f32 = 190.0;

/// The lane's keyboard target.
pub(super) fn focus_id() -> egui::Id {
    egui::Id::NULL.with("trace-lane-keyboard")
}

#[derive(Default)]
pub(super) struct Lane {
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
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Glyph {
    Running,
    Ok,
    Error,
}

/// One line under a card's header.
#[derive(Clone, PartialEq, Debug)]
pub(super) enum Line {
    /// A ticket of the entry's cards, by index.
    Ticket(usize),
    Text(String),
}

/// A trace entry as the lane shows it, before any painting.
#[derive(Clone, PartialEq, Debug)]
pub(super) struct CardView {
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

pub(super) fn elapsed_text(ms: u64) -> String {
    format!("{:.1}s", ms as f64 / 1000.0)
}

/// What a trace entry looks like as a card, by kind (design §2.3, §5.4).
pub(super) fn card_view(entry: &TraceEntry) -> CardView {
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
pub(super) fn shown_lines(view: &CardView, expanded: bool, docked: bool) -> Vec<Line> {
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

impl State {
    pub(super) fn chief_session_id(&self) -> Option<&str> {
        self.sessions.iter().find(|s| s.chief).map(|s| s.id.as_str())
    }

    /// The chief's trace entries still in view, oldest first.
    pub(super) fn lane_entries(&self) -> Vec<&TraceEntry> {
        let Some(log) = self.chief_session_id().and_then(|id| self.traces.get(id)) else { return Vec::new() };
        log.entries.iter().filter(|entry| entry.seq > self.lane.cleared).collect()
    }

    /// Shift+↓/↑ with the lane focused: the next or previous card.
    pub(super) fn lane_step(&mut self, delta: i32) {
        let seqs: Vec<u64> = self.lane_entries().iter().map(|entry| entry.seq).collect();
        let current = self.lane.selected.and_then(|seq| seqs.iter().position(|s| *s == seq));
        self.lane.selected = panel_nav::step_index(current, seqs.len(), delta).map(|i| seqs[i]);
        self.lane.reveal = true;
    }

    /// The newest card as one line, for the Tickets panel's footer.
    pub(super) fn lane_echo(&self) -> Option<String> {
        let entry = *self.lane_entries().last()?;
        let view = card_view(entry);
        let mark = match view.glyph {
            Glyph::Running => "…",
            Glyph::Ok => "✓",
            Glyph::Error => "✗",
        };
        Some(format!("chief · {} {mark}", view.title))
    }

    /// Opens the chief's window and focuses its terminal.
    pub(super) fn show_chief(&mut self) {
        self.chief_open = true;
        self.focus_terminal = self.chief_session_id().map(str::to_string);
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

enum Act {
    Toggle(u64),
    Select(u64),
    Open(Box<TicketLink>),
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
    config: Option<&TicketConfig>,
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
    let key = fit(ui, card.link.label(), p.mono(11.5), p.accent, rect.right() - x);
    let width = key.size().x;
    painter.galley(pos2(x, y), key, p.accent);
    x += width + 8.0;
    if let Some(state) = &card.link.state {
        let color = config.map_or(p.muted, |config| state_color(config, state, p));
        x = chip(ui, &painter, pos2(x, y), state, color, rect.right(), p);
    }
    if x < rect.right() - 12.0
        && let Some(title) = card.link.title.as_deref()
    {
        let galley = fit(ui, title, p.ui(11.5), p.foreground, rect.right() - x);
        painter.galley(pos2(x, y), galley, p.foreground);
    }
}

impl Desktop<'_> {
    /// Height the lane takes in a column `available` tall, 0 when the chief has no trace in view.
    pub(super) fn lane_height(&self, available: f32) -> f32 {
        if self.runtime.lane_entries().is_empty() {
            return 0.0;
        }
        if self.runtime.lane.collapsed {
            return HEAD_H + 4.0;
        }
        self.runtime.lane.height.unwrap_or(DEFAULT_H).clamp(MIN_H, (available * 0.7).max(MIN_H))
    }

    /// Draws the lane in `rect`. `docked` collapses cards to a line each.
    pub(super) fn trace_lane(&mut self, ui: &mut egui::Ui, rect: Rect, docked: bool) {
        let p = self.runtime.palette.clone();
        let config = self.runtime.ticket_config();
        let now = now_ms();
        let time = ui.input(|i| i.time);
        let mut acts: Vec<Act> = Vec::new();
        ui.painter().rect_filled(rect, 0.0, lerp(p.terminal, p.raised, 0.25));
        ui.painter().line_segment([rect.left_top(), rect.right_top()], Stroke::new(1.0, p.border));
        let lane = &mut self.runtime.lane;
        let chief = self.runtime.sessions.iter().find(|s| s.chief).map(|s| s.id.clone()).unwrap_or_default();
        let entries: Vec<&TraceEntry> = self
            .runtime
            .traces
            .get(&chief)
            .map(|log| log.entries.iter().filter(|entry| entry.seq > lane.cleared).collect())
            .unwrap_or_default();
        let mut inner =
            ui.new_child(egui::UiBuilder::new().id_salt("trace-lane").max_rect(rect.shrink2(vec2(6.0, 2.0))));
        let ui = &mut inner;
        ui.set_clip_rect(ui.clip_rect().intersect(rect));
        let running = entries.iter().any(|entry| entry.state == TraceState::Running);
        let focused = ui.memory(|m| m.has_focus(focus_id()));
        ui.horizontal(|ui| {
            ui.set_height(HEAD_H - 2.0);
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            let plan = entries.iter().rev().find_map(|entry| entry.plan_id.clone());
            let label = plan.map_or("TRACE".to_string(), |plan| format!("TRACE · plan {plan}"));
            let color = if focused { p.accent } else { p.muted };
            ui.label(RichText::new(label).size(10.5).strong().color(color));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button(if lane.collapsed { "▴" } else { "▾" })
                    .on_hover_text("Collapse or expand the trace")
                    .clicked()
                {
                    acts.push(Act::Collapse);
                }
                if ui.small_button("clear").clicked() {
                    acts.push(Act::Clear);
                }
                let calls = entries.len();
                let total: u64 = entries.iter().filter_map(|entry| entry.elapsed_ms).sum();
                ui.label(
                    RichText::new(format!(
                        "{calls} call{} · {}",
                        if calls == 1 { "" } else { "s" },
                        elapsed_text(total)
                    ))
                    .size(11.0)
                    .color(p.muted),
                );
            });
        });
        if !lane.collapsed {
            let body = Rect::from_min_max(pos2(rect.left(), rect.top() + HEAD_H), rect.right_bottom());
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
            let jump = panel_nav::keyboard_scroll_for(
                ui,
                focus_id(),
                scroll_id,
                LINE_H,
                0..rows,
                &panel_nav::SCROLL_KEYS,
                jump,
            );
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
                    let (card, response) =
                        ui.allocate_exact_size(vec2(width, card_height(lines.len())), Sense::click());
                    let painter = ui.painter().with_clip_rect(card.intersect(ui.clip_rect()));
                    let selected = lane.selected == Some(entry.seq);
                    if selected {
                        painter.rect_filled(card.shrink2(vec2(0.0, 1.0)), 3.0, lerp(p.raised, p.accent, 0.14));
                        painter.rect_filled(
                            Rect::from_min_size(card.left_top(), vec2(3.0, card.height())),
                            0.0,
                            p.accent,
                        );
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
                            acts.push(Act::Open(Box::new(first.link.clone())));
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
                        let galley = fit(ui, &view.detail, p.ui(11.5), p.muted, meta_left - x - title_w - 10.0);
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
                                ticket_row(ui, row, ticket, range, config.as_deref(), &p, hit.hovered());
                                if hit.clicked() {
                                    acts.push(Act::Open(Box::new(ticket.link.clone())));
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
                                let galley = fit(ui, text, p.ui(11.5), color, row.width());
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
                    if !self.runtime.lane.expanded.remove(&seq) {
                        self.runtime.lane.expanded.insert(seq);
                    }
                }
                Act::Select(seq) => self.runtime.lane.selected = Some(seq),
                Act::Open(link) => self.runtime.open_ticket(*link, super::tickets_panel::Nav::Fresh),
                Act::Clear => {
                    self.runtime.lane.cleared = self
                        .runtime
                        .traces
                        .get(&chief)
                        .and_then(|log| log.entries.back().map(|entry| entry.seq))
                        .unwrap_or(0);
                    self.runtime.lane.selected = None;
                }
                Act::Collapse => self.runtime.lane.collapsed = !self.runtime.lane.collapsed,
                Act::Focus => ui.memory_mut(|m| m.request_focus(focus_id())),
            }
        }
    }
}

/// The handle above the lane: drag to resize, double-click to reset. Returns the new height.
pub(super) fn lane_divider(ui: &mut egui::Ui, rect: Rect, height: &mut Option<f32>, p: &Palette) {
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

#[cfg(test)]
mod tests {
    use super::super::tests::{session, with_app};
    use super::*;
    use crate::ipc::HookMessage;
    use crate::mcp_client::tests::{example_profile, fake_server};
    use atomics_core::trace::{TracePhase, TracePlan, TraceTask};
    use serde_json::{Value, json};

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

    fn hook(name: &str, tool: &str, id: &str, input: Value, response: Value) -> HookMessage {
        HookMessage {
            pane: Some("chief".into()),
            ancestors: Vec::new(),
            delivered: 0,
            approval_reply: None,
            event: atomics_core::hook::HookEvent::from_payload(&json!({
                "hook_event_name":name, "session_id":"chief", "tool_name":format!("mcp__atomics__{tool}"),
                "tool_use_id":id, "tool_input":input, "tool_response":response
            }))
            .unwrap(),
        }
    }

    fn settle(app: &mut Desktop<'_>) {
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(30));
        loop {
            app.runtime.ticket_results();
            if app.runtime.tickets.pending == 0 {
                return;
            }
            assert!(Instant::now() < deadline, "ticket jobs did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The chief calls one of its ticket tools: ATOMICS runs it on the fake server and the hooks report it.
    fn call(app: &mut Desktop<'_>, tool: &str, input: Value) -> Value {
        let (tx, rx) = std::sync::mpsc::channel();
        let request = crate::mcp::request_for_tool(tool, input.clone()).unwrap();
        app.runtime.on_hook(hook("PreToolUse", tool, tool, input.clone(), Value::Null));
        app.runtime.control(
            &app.runtime.ctx.clone(),
            serde_json::to_value(request).and_then(serde_json::from_value).unwrap(),
            tx,
        );
        settle(app);
        let crate::ipc::ControlOutcome::Done(response) =
            rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(2))).unwrap()
        else {
            panic!("deferred")
        };
        assert!(response.ok, "{response:?}");
        app.runtime.on_hook(hook("PostToolUse", tool, tool, input, json!({"structuredContent": response.result})));
        response.result
    }

    fn fake_profile(app: &mut Desktop<'_>, dir: &Path) {
        let state = dir.join("fake.json");
        let mut config = example_profile("meaning-example", &state);
        config.server.command = vec![fake_server().display().to_string(), state.display().to_string()];
        config.operations.children = Some(
            toml::from_str(
                r#"
                tool = "get_children"
                arguments = { ticket = "{key}" }
                items = "result.tickets"
                result = { id = "id", key = "number", title = "subject", state = "status", assignee = "assignee" }
                "#,
            )
            .unwrap(),
        );
        app.runtime.config.tickets = config;
        app.runtime.apply_ticket_config();
    }

    fn chief_texts(
        app: &mut Desktop<'_>,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
    ) -> (egui::FullOutput, Vec<(Pos2, String)>) {
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(900.0, 700.0))),
                events,
                ..Default::default()
            },
            |ui| {
                app.shortcuts(ui.ctx());
                app.chief_content(ui);
            },
        );
        app.runtime.drain_actions();
        let texts = output
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::epaint::Shape::Text(t) => Some((t.pos, t.galley.text().to_string())),
                _ => None,
            })
            .collect();
        (output, texts)
    }

    fn scripted_chief(app: &mut Desktop<'_>, dir: &Path) {
        fake_profile(app, dir);
        let root = app.runtime.launch_dir.clone();
        let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &root);
        chief.chief = true;
        app.runtime.sessions = vec![chief];
        app.runtime.chief_open = true;
        app.runtime.store.chief_dock = Some(ChiefDock::Floating);
    }

    #[test]
    fn trace_lane_shows_search_read_children_related_and_plan_cards_in_order() {
        with_app(
            "app::trace_lane::tests::trace_lane_shows_search_read_children_related_and_plan_cards_in_order",
            |app, ctx| {
                let dir = crate::test_support::tempdir().unwrap();
                scripted_chief(app, dir.path());
                let found = call(
                    app,
                    "atomics_ticket_search",
                    json!({"query":"personal-agent ticket workspace", "semantic":true}),
                );
                let epic = found["cards"][0]["key"].as_str().unwrap().to_string();
                call(app, "atomics_ticket_read", json!({"ticket":"DEMO-1"}));
                call(app, "atomics_ticket_children", json!({"ticket":"DEMO-1"}));
                call(app, "atomics_ticket_related", json!({"ticket":"DEMO-1"}));
                let plan = json!({"goal":"Epic", "plan":"epic", "tasks":[
                {"id":"t1", "title":"First", "ticket":{"id":"x", "key":"DEMO-2"}},
                {"id":"t2", "title":"Second", "depends_on":["t1"]}]});
                // The graph is on screen at Pre time, before the reply.
                app.runtime.on_hook(hook("PreToolUse", "atomics_plan_set", "plan-1", plan.clone(), Value::Null));
                let running = app.runtime.lane_entries().last().map(|e| card_view(e)).unwrap();
                assert_eq!((running.glyph, running.meta.as_str()), (Glyph::Running, "2 tasks"));
                app.runtime.on_hook(hook(
                    "PostToolUse",
                    "atomics_plan_set",
                    "plan-1",
                    plan,
                    json!({"structuredContent":{"plan":"epic", "tasks":2}}),
                ));

                let views: Vec<CardView> = app.runtime.lane_entries().into_iter().map(card_view).collect();
                let kinds: Vec<&str> = views.iter().map(|v| v.kind).collect();
                assert_eq!(kinds, ["search", "read", "children", "related", "plan"], "{views:#?}");
                assert!(views.iter().all(|v| v.glyph == Glyph::Ok));
                assert_eq!(views[0].title, "search by meaning");
                assert_eq!(views[1].title, "read DEMO-1");
                assert_eq!(views[2].meta, "4 tickets");
                assert_eq!(views[0].open, Some(0));
                assert_eq!(views[4].meta, "2 tasks");

                let (_, texts) = chief_texts(app, ctx, vec![]);
                let all = texts.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>().join("\n");
                for expected in
                    ["TRACE", "search by meaning", "read DEMO-1", "children of DEMO-1", "related to DEMO-1", "5 calls"]
                {
                    assert!(all.contains(expected), "{expected} missing in {all}");
                }
                let at =
                    |needle: &str| texts.iter().find(|(_, t)| t.contains(needle)).map(|(p, _)| p.y).unwrap_or(f32::MAX);
                assert!(
                    at("search by meaning") < at("read DEMO-1") && at("read DEMO-1") < at("children of"),
                    "{texts:?}"
                );
                // The footer echo names the newest card.
                assert!(app.runtime.lane_echo().unwrap().starts_with("chief · plan epic"));

                // A click on "open ↗" of the search card reads its first result in the panel.
                let open = texts.iter().find(|(_, t)| t == "open ↗").expect("open link").0;
                let click = open + vec2(4.0, 4.0);
                let button = |pressed| egui::Event::PointerButton {
                    pos: click,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                };
                chief_texts(app, ctx, vec![egui::Event::PointerMoved(click)]);
                chief_texts(app, ctx, vec![button(true)]);
                chief_texts(app, ctx, vec![button(false)]);
                settle(app);
                assert!(app.runtime.panel == PanelMode::Tickets);
                assert_eq!(app.runtime.tickets.panel.read.as_ref().map(|r| r.link.label().to_string()), Some(epic));
            },
        );
    }

    #[test]
    fn trace_lane_steps_cards_with_shift_arrows_when_focused() {
        with_app("app::trace_lane::tests::trace_lane_steps_cards_with_shift_arrows_when_focused", |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            scripted_chief(app, dir.path());
            for tool in ["atomics_ticket_read", "atomics_ticket_children", "atomics_ticket_related"] {
                call(app, tool, json!({"ticket":"DEMO-1"}));
            }
            let seqs: Vec<u64> = app.runtime.lane_entries().iter().map(|e| e.seq).collect();
            chief_texts(app, ctx, vec![]);
            ctx.memory_mut(|m| m.request_focus(focus_id()));
            chief_texts(app, ctx, vec![]);
            assert!(ctx.memory(|m| m.has_focus(focus_id())));
            let shift = |key| {
                vec![
                    egui::Event::ModifiersChanged(egui::Modifiers::SHIFT),
                    egui::Event::Key {
                        key,
                        physical_key: Some(key),
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::SHIFT,
                    },
                ]
            };
            chief_texts(app, ctx, shift(egui::Key::ArrowDown));
            assert_eq!(app.runtime.lane.selected, Some(seqs[0]));
            chief_texts(app, ctx, shift(egui::Key::ArrowDown));
            chief_texts(app, ctx, shift(egui::Key::ArrowDown));
            chief_texts(app, ctx, shift(egui::Key::ArrowDown));
            assert_eq!(app.runtime.lane.selected, Some(seqs[2]), "clamps at the last card");
            chief_texts(app, ctx, shift(egui::Key::ArrowUp));
            assert_eq!(app.runtime.lane.selected, Some(seqs[1]));
            // Enter expands the selected card; Escape hands the focus to the terminal.
            let enter = egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: Some(egui::Key::Enter),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            };
            chief_texts(app, ctx, vec![egui::Event::ModifiersChanged(egui::Modifiers::NONE), enter]);
            assert!(app.runtime.lane.expanded.contains(&seqs[1]));
            let escape = egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: Some(egui::Key::Escape),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            };
            chief_texts(app, ctx, vec![escape]);
            assert!(!ctx.memory(|m| m.has_focus(focus_id())));
            assert_eq!(app.client.focus_terminal.as_deref(), Some("chief"));
        });
    }

    #[test]
    fn trace_lane_clears_and_drops_secrets_never_reach_the_cards() {
        with_app("app::trace_lane::tests::trace_lane_clears_and_drops_secrets_never_reach_the_cards", |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            scripted_chief(app, dir.path());
            app.runtime.on_hook(hook(
                "PostToolUse",
                "other_lookup",
                "x",
                json!({"q":"hello", "api_key":"sk-live-1234567890abcdef"}),
                json!({"error":"unauthorized"}),
            ));
            let (_, texts) = chief_texts(app, ctx, vec![]);
            let all = texts.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>().join("\n");
            assert!(all.contains("atomics: other_lookup") && all.contains("error: unauthorized"), "{all}");
            assert!(!all.contains("sk-live"));
            app.runtime.lane.cleared = app.runtime.lane_entries().last().unwrap().seq;
            assert!(app.runtime.lane_entries().is_empty());
            assert_eq!(app.lane_height(600.0), 0.0);
        });
    }
}
