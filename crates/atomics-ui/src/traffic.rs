//! The traffic in a plan's flow: what passes between the chief and its agents, and between agents, as pulses
//! along the flow's curves and small marks where they land. It is made only from what the host already
//! records, compared frame by frame with what this client saw before:
//!
//! - the chief's `atomics_send` and `atomics_send_review` calls in its trace: a pulse from the chief to the task;
//! - a task's report (its time changed): a pulse from the task to the chief, and its track updates when it lands;
//! - a note an agent proposes in the plan's notebook: a mark on its row; one pinned: a mark on the chief's node;
//! - an addressed message or a broadcast between agents: a pulse from row to row, past the spine. A message the
//!   relay holds waits at the chief's node until it is approved (it goes on) or denied (it ends there).
//!
//! What this client first sees is only remembered, never played: a page that connects, or connects again, shows
//! the plan as it stands. After that, only what happened within [`RECENT`] seconds of the host's clock moves, at
//! most [`MAX_PULSES`] pulses at once, and every pulse and mark ends after its window. With reduced motion
//! nothing moves: what would have travelled is at once a mark where it lands.

use std::collections::{HashMap, HashSet};

use atomics_core::notebook::{Kind as NoteKind, RelayState};
use atomics_core::plan::{Plan, Report, TaskStatus};
use atomics_core::trace_feed::TraceData;

/// Seconds a pulse takes from one end to the other.
pub const FLIGHT: f64 = 1.4;
/// Seconds a mark stays where a pulse landed; it fades over the last third.
pub const MARK: f64 = 45.0;
/// How old, by the host's clock, a change may be and still move: nothing older replays.
pub const RECENT: u64 = 30;
/// The most pulses in flight at once; the oldest give way.
pub const MAX_PULSES: usize = 4;
/// The most events kept, pulses and marks together.
pub const MAX_EVENTS: usize = 24;
/// The longest text kept from an agent or the chief, in characters.
pub const MAX_TEXT: usize = 240;

/// One end of a pulse: the chief's node, or a task's row by id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum End {
    Chief,
    Task(String),
}

/// What passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dispatch,
    Review,
    /// The chief sent the task's agent a message, or a review.
    Send,
    /// The task's agent reported: progress, a question, blocked or done.
    Report,
    /// An agent proposed a note for the plan's notebook.
    Note,
    /// A note was pinned.
    Pinned,
    /// An agent's message to another, or a broadcast's copy.
    Message,
}

/// What became of a message the relay held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Held {
    /// Not held.
    No,
    /// Waits at the chief's node.
    Waiting,
    /// Denied or expired: it ends at the node.
    Stopped,
}

/// One thing that passed, and when this client began to show it.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// What it is, to update it: `report:t1`, `note:12`, `send:7`.
    pub key: String,
    pub kind: Kind,
    pub from: End,
    pub to: End,
    /// This client's clock, in seconds, when it began to travel.
    pub started: f64,
    pub held: Held,
    /// Who sent it, as the row names them ("t1 · Parse faster") or "Chief".
    pub who: String,
    /// What it said, cut to [`MAX_TEXT`]: an agent's own words where `agent` is set.
    pub text: String,
    /// The text is an agent's, which is data and not instructions.
    pub agent: bool,
    /// The host's clock when it happened, in seconds.
    pub at: u64,
}

/// A pulse on its way: how far along, from 0 at its start to 1 where it lands.
#[derive(Debug, Clone, PartialEq)]
pub struct Pulse<'a> {
    pub event: &'a Event,
    pub progress: f32,
}

/// What the traffic is made from, as one frame reads it.
pub struct Observed<'a> {
    pub plan: &'a Plan,
    /// The chief's trace, for its sends.
    pub trace: &'a TraceData,
    /// The host's clock, in seconds.
    pub host_now: u64,
}

/// What one client has seen of a plan's traffic, and what it shows of it.
#[derive(Debug, Clone, Default)]
pub struct Traffic {
    /// The plan it follows; another plan starts over.
    plan: Option<String>,
    /// Each task's last report, as last seen.
    reports: HashMap<String, Report>,
    tasks: HashMap<String, (TaskStatus, Option<String>)>,
    /// Each notebook entry seen, by id: pinned, and its relay's state.
    notes: HashMap<u64, (bool, Option<RelayState>)>,
    /// The chief's trace entries seen, by sequence number.
    calls: HashSet<u64>,
    /// A task's report on its way to the chief: when it set off, and the report its track shows meanwhile.
    landing: HashMap<String, (f64, Option<Report>)>,
    pub events: Vec<Event>,
}

fn cut(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(MAX_TEXT) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

impl Traffic {
    /// Compares what the host says now with what this client saw before, and starts what changed. The first
    /// look at a plan only remembers it.
    pub fn observe(&mut self, seen: &Observed, now: f64) {
        let plan = seen.plan;
        let first = self.plan.as_deref() != Some(plan.id.as_str());
        if first {
            *self = Traffic { plan: Some(plan.id.clone()), ..Traffic::default() };
        }
        if !plan.is_open() {
            self.events.clear();
            self.landing.clear();
            return;
        }
        let recent = |at: u64| seen.host_now.saturating_sub(at) <= RECENT;
        let name = |task: &str| match plan.tasks.iter().find(|t| t.id == task) {
            Some(t) => format!("{} · {}", t.id, t.title),
            None => task.to_string(),
        };
        let mut started = Vec::new();
        for task in &plan.tasks {
            let state = (task.status, task.session.clone());
            let previous = self.tasks.insert(task.id.clone(), state.clone());
            let handoff = match (&previous, task.status) {
                _ if first || previous.as_ref() == Some(&state) => None,
                (_, TaskStatus::Running) if task.session.is_some() => Some(Kind::Dispatch),
                (Some(_), TaskStatus::Review)
                    if task.report.as_ref().is_none_or(|r| self.reports.get(&task.id) == Some(r)) =>
                {
                    Some(Kind::Review)
                }
                _ => None,
            };
            if let Some(kind) = handoff {
                let dispatched = kind == Kind::Dispatch;
                let end = End::Task(task.id.clone());
                let (from, to, who) =
                    if dispatched { (End::Chief, end, "Chief".into()) } else { (end, End::Chief, name(&task.id)) };
                started.push(Event {
                    key: format!("handoff:{}", task.id),
                    kind,
                    from,
                    to,
                    started: now,
                    held: Held::No,
                    who,
                    text: if dispatched { "Task dispatched" } else { "Ready for review" }.into(),
                    agent: false,
                    at: seen.host_now,
                });
            }
            let Some(report) = &task.report else { continue };
            if self.reports.get(&task.id) == Some(report) {
                continue;
            }
            let before = self.reports.insert(task.id.clone(), report.clone());
            if first || !recent(report.at) {
                continue;
            }
            let summary = report.summary.lines().next().unwrap_or_default();
            self.landing.insert(task.id.clone(), (now, before));
            started.push(Event {
                key: format!("report:{}", task.id),
                kind: Kind::Report,
                from: End::Task(task.id.clone()),
                to: End::Chief,
                started: now,
                held: Held::No,
                who: name(&task.id),
                text: cut(&format!("{}: {summary}", report.status)),
                agent: true,
                at: report.at,
            });
        }
        let tasks: Vec<&str> = plan.tasks.iter().map(|t| t.id.as_str()).collect();
        for note in &plan.notebook {
            let relay = note.relay.as_ref().map(|r| r.state);
            let before = self.notes.insert(note.id, (note.pinned, relay));
            let text = if note.hidden { "(hidden message)".to_string() } else { cut(&note.text) };
            let key = format!("note:{}", note.id);
            match before {
                None if first || !recent(note.at) => {}
                None => match note.kind {
                    NoteKind::Fact => started.push(Event {
                        key,
                        kind: Kind::Note,
                        from: End::Task(note.task.clone()),
                        to: End::Task(note.task.clone()),
                        started: now,
                        held: Held::No,
                        who: name(&note.task),
                        text,
                        agent: true,
                        at: note.at,
                    }),
                    NoteKind::Message | NoteKind::Broadcast => {
                        let to: Vec<String> = match (&note.kind, &note.to) {
                            (NoteKind::Message, Some(to)) => vec![to.clone()],
                            _ => tasks.iter().filter(|t| **t != note.task).map(|t| t.to_string()).collect(),
                        };
                        let held = if relay == Some(RelayState::Pending) { Held::Waiting } else { Held::No };
                        for (n, to) in to.into_iter().take(MAX_PULSES).enumerate() {
                            started.push(Event {
                                key: format!("{key}:{n}"),
                                kind: Kind::Message,
                                from: End::Task(note.task.clone()),
                                to: if held == Held::Waiting { End::Chief } else { End::Task(to) },
                                started: now,
                                held,
                                who: name(&note.task),
                                text: text.clone(),
                                agent: true,
                                at: note.at,
                            });
                        }
                    }
                },
                Some((was_pinned, was_relay)) => {
                    if !was_pinned && note.pinned {
                        started.push(Event {
                            key: format!("{key}:pinned"),
                            kind: Kind::Pinned,
                            from: End::Chief,
                            to: End::Chief,
                            started: now,
                            held: Held::No,
                            who: name(&note.task),
                            text: text.clone(),
                            agent: true,
                            at: seen.host_now,
                        });
                    }
                    if was_relay == Some(RelayState::Pending) && relay != Some(RelayState::Pending) {
                        self.decided(&key, note.to.as_deref(), relay == Some(RelayState::Approved), now);
                    }
                }
            }
        }
        for entry in &seen.trace.entries {
            if !self.calls.insert(entry.seq) || first || entry.turn.is_some() {
                continue;
            }
            if entry.server != "atomics" || !matches!(entry.tool.as_str(), "atomics_send" | "atomics_send_review") {
                continue;
            }
            if seen.host_now.saturating_mul(1000).saturating_sub(entry.at) > RECENT * 1000 {
                continue;
            }
            let args: serde_json::Value = serde_json::from_str(&entry.summary).unwrap_or_default();
            let Some(session) = args["session"].as_str() else { continue };
            let Some(task) = plan.tasks.iter().find(|t| t.session.as_deref() == Some(session)) else { continue };
            let text = if entry.tool == "atomics_send_review" {
                "Review comments".to_string()
            } else {
                cut(args["text"].as_str().unwrap_or_default())
            };
            started.push(Event {
                key: format!("send:{}", entry.seq),
                kind: Kind::Send,
                from: End::Chief,
                to: End::Task(task.id.clone()),
                started: now,
                held: Held::No,
                who: "Chief".into(),
                text,
                agent: false,
                at: entry.at / 1000,
            });
        }
        self.events.extend(started);
        self.bound(now);
    }

    /// A held message was decided: approved, it goes on from the chief's node to its recipient; denied or
    /// expired, it stops there.
    fn decided(&mut self, key: &str, to: Option<&str>, approved: bool, now: f64) {
        let prefix = format!("{key}:");
        let mut onward = Vec::new();
        for event in self.events.iter_mut().filter(|e| e.key.starts_with(&prefix) && e.held == Held::Waiting) {
            if approved && let Some(to) = to {
                onward.push(Event {
                    key: format!("{}:on", event.key),
                    from: End::Chief,
                    to: End::Task(to.to_string()),
                    started: now,
                    held: Held::No,
                    ..event.clone()
                });
                event.held = Held::No;
            } else {
                event.held = Held::Stopped;
            }
            // What it was waiting for is over: its mark lasts from now.
            event.started = now - FLIGHT;
        }
        self.events.extend(onward);
    }

    /// Ends what is over, and keeps at most [`MAX_PULSES`] in flight and [`MAX_EVENTS`] in all.
    fn bound(&mut self, now: f64) {
        self.events.retain(|e| e.held == Held::Waiting || now - e.started < FLIGHT + MARK);
        self.landing.retain(|_, (started, _)| now - *started < FLIGHT);
        let flying: Vec<usize> =
            (0..self.events.len()).filter(|&i| now - self.events[i].started < FLIGHT && self.moves(i)).collect();
        // The oldest pulses land at once, as marks.
        for &i in flying.iter().take(flying.len().saturating_sub(MAX_PULSES)) {
            self.events[i].started = now - FLIGHT;
        }
        let over = self.events.len().saturating_sub(MAX_EVENTS);
        self.events.drain(..over);
    }

    /// Whether the event at `index` travels: a note's mark does not.
    fn moves(&self, index: usize) -> bool {
        !matches!(self.events[index].kind, Kind::Note | Kind::Pinned)
    }

    /// The pulses on their way at `now`, and the messages held at the chief's node (at the end of their way).
    /// None with reduced motion: then everything is at once a mark.
    pub fn pulses(&self, now: f64, reduce_motion: bool) -> Vec<Pulse<'_>> {
        if reduce_motion {
            return Vec::new();
        }
        self.events
            .iter()
            .enumerate()
            .filter(|(i, _)| self.moves(*i))
            .filter_map(|(_, event)| {
                let progress = ((now - event.started) / FLIGHT) as f32;
                match event.held {
                    Held::Waiting => Some(Pulse { event, progress: progress.min(1.0) }),
                    _ => (0.0..1.0).contains(&progress).then_some(Pulse { event, progress }),
                }
            })
            .collect()
    }

    /// What has landed, with how strongly its mark shows at `now` (1, fading to 0 at the end of its window).
    /// With reduced motion, what is on its way has landed already.
    pub fn marks(&self, now: f64, reduce_motion: bool) -> Vec<(&Event, f32)> {
        self.events
            .iter()
            .enumerate()
            .filter_map(|(i, event)| {
                let since = now - event.started - if reduce_motion || !self.moves(i) { 0.0 } else { FLIGHT };
                if event.held == Held::Waiting {
                    return reduce_motion.then_some((event, 1.0));
                }
                if !(0.0..MARK).contains(&since) {
                    return None;
                }
                let fade = ((MARK - since) / (MARK / 3.0)).min(1.0) as f32;
                Some((event, fade))
            })
            .collect()
    }

    /// Whether anything still moves or fades at `now`, so the window draws again.
    pub fn active(&self, now: f64) -> bool {
        self.events.iter().any(|e| e.held == Held::Waiting || now - e.started < FLIGHT + MARK)
    }

    /// Whether a pulse is still on its way at `now`: the window then draws every frame.
    pub fn moving(&self, now: f64, reduce_motion: bool) -> bool {
        !reduce_motion && self.pulses(now, false).iter().any(|pulse| pulse.progress < 1.0)
    }

    /// The report a task's track shows at `now`: the one before while the new one is on its way to the chief.
    pub fn shown_report<'a>(
        &'a self,
        task: &str,
        current: Option<&'a Report>,
        now: f64,
        reduce_motion: bool,
    ) -> Option<&'a Report> {
        match self.landing.get(task) {
            Some((started, before)) if !reduce_motion && now - started < FLIGHT => before.as_ref(),
            _ => current,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomics_core::plan::{NotebookEntry, Task, TaskStatus};
    use atomics_core::trace::{TraceEntry, TraceOp, TracePhase, TraceState};

    fn plan() -> Plan {
        let task = |id: &str| Task {
            id: id.into(),
            title: format!("Task {id}"),
            status: TaskStatus::Running,
            session: Some(format!("s-{id}")),
            ..Task::default()
        };
        Plan { id: "p1".into(), tasks: vec![task("t1"), task("t2"), task("t3")], ..Plan::default() }
    }

    fn report(at: u64, status: &str) -> Report {
        Report { status: status.into(), summary: format!("{status} at {at}\nmore"), at, checks: Vec::new() }
    }

    fn note(id: u64, kind: NoteKind, task: &str, to: Option<&str>, at: u64) -> NotebookEntry {
        NotebookEntry {
            id,
            task: task.into(),
            agent: "claude".into(),
            at,
            text: format!("note {id}"),
            pinned: false,
            kind,
            session: String::new(),
            to: to.map(str::to_string),
            reply_to: None,
            hidden: false,
            pending: Vec::new(),
            delivered: Default::default(),
            relay: None,
        }
    }

    fn send(seq: u64, session: &str, at_ms: u64) -> TraceEntry {
        TraceEntry {
            chat_id: None,
            session_id: "c".into(),
            seq,
            at: at_ms,
            phase: TracePhase::Post,
            tool_use_id: None,
            server: "atomics".into(),
            tool: "atomics_send".into(),
            op: TraceOp::Other,
            direct: false,
            plan_id: None,
            summary: serde_json::json!({"session": session, "text": "Rebase on main"}).to_string(),
            state: TraceState::Ok,
            elapsed_ms: None,
            cards: Vec::new(),
            plan: None,
            result_bytes: 0,
            error: None,
            hint: None,
            truncated: false,
            omitted: 0,
            turn: None,
        }
    }

    fn look(traffic: &mut Traffic, plan: &Plan, trace: &TraceData, host_now: u64, now: f64) {
        traffic.observe(&Observed { plan, trace, host_now }, now);
    }

    #[test]
    fn dispatch_and_review_send_one_pulse_in_the_direction_of_the_handoff() {
        let mut t = Traffic::default();
        let mut p = plan();
        p.tasks[0].status = TaskStatus::Planned;
        p.tasks[0].session = None;
        let trace = TraceData::default();
        look(&mut t, &p, &trace, 1000, 0.0);
        p.tasks[0].status = TaskStatus::Running;
        p.tasks[0].session = Some("new-session".into());
        look(&mut t, &p, &trace, 1001, 1.0);
        let outgoing = t.pulses(1.25, false);
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0].event.from, End::Chief);
        assert_eq!(outgoing[0].event.to, End::Task("t1".into()));
        p.tasks[0].status = TaskStatus::Review;
        p.tasks[0].report = Some(report(1004, "done"));
        look(&mut t, &p, &trace, 1004, 4.0);
        look(&mut t, &p, &trace, 1004, 4.1);
        let incoming = t.pulses(4.2, false);
        assert_eq!(incoming.len(), 1, "one completed handoff, including its report");
        assert_eq!(incoming[0].event.to, End::Chief);
        assert!(t.pulses(4.2, true).is_empty());
    }

    #[test]
    fn the_first_look_remembers_and_only_later_changes_move_then_stop_after_their_window() {
        let mut traffic = Traffic::default();
        let mut plan = plan();
        plan.tasks[0].report = Some(report(990, "progress"));
        let trace = TraceData { entries: vec![send(1, "s-t2", 995_000)], ..TraceData::default() };
        look(&mut traffic, &plan, &trace, 1000, 10.0);
        assert!(traffic.events.is_empty(), "what the client first sees is not played");
        assert!(!traffic.active(10.0));

        // A report arrives: one pulse from the task to the chief.
        plan.tasks[0].report = Some(report(1001, "done"));
        look(&mut traffic, &plan, &trace, 1001, 20.0);
        let pulses = traffic.pulses(20.5, false);
        assert_eq!(pulses.len(), 1);
        let pulse = &pulses[0];
        assert_eq!(
            (pulse.event.kind, &pulse.event.from, &pulse.event.to),
            (Kind::Report, &End::Task("t1".into()), &End::Chief)
        );
        assert!(pulse.progress > 0.3 && pulse.progress < 0.4);
        assert_eq!(pulse.event.text, "done: done at 1001", "the first line, framed by its status");
        assert!(pulse.event.agent);
        assert!(traffic.moving(20.5, false));
        // It lands: a mark stays for its window, then everything is over and nothing draws again.
        assert!(traffic.pulses(20.0 + FLIGHT + 0.01, false).is_empty());
        assert_eq!(traffic.marks(20.0 + FLIGHT + 1.0, false).len(), 1);
        assert!(!traffic.moving(20.0 + FLIGHT + 1.0, false));
        assert!(traffic.active(20.0 + FLIGHT + MARK - 0.1));
        assert!(traffic.marks(20.0 + FLIGHT + MARK + 0.1, false).is_empty());
        assert!(!traffic.active(20.0 + FLIGHT + MARK + 0.1));
        // While it travels the track shows the report before; it updates when the pulse lands.
        let current = plan.tasks[0].report.as_ref();
        assert_eq!(traffic.shown_report("t1", current, 20.5, false).map(|r| r.at), Some(990));
        assert_eq!(traffic.shown_report("t1", current, 20.5, true).map(|r| r.at), Some(1001));
        assert_eq!(traffic.shown_report("t1", current, 20.0 + FLIGHT + 0.01, false).map(|r| r.at), Some(1001));
        // The same report seen again starts nothing.
        look(&mut traffic, &plan, &trace, 1002, 30.0);
        assert_eq!(traffic.events.len(), 1);
    }

    #[test]
    fn the_chiefs_sends_go_to_the_tasks_session_and_old_ones_never_replay() {
        let mut traffic = Traffic::default();
        let plan = plan();
        let mut trace = TraceData::default();
        look(&mut traffic, &plan, &trace, 1000, 0.0);
        trace.entries.push(send(2, "s-t2", 1_000_000));
        // Seen first long after it happened (a page that reconnects): remembered, not played.
        trace.entries.push(send(3, "s-t3", 900_000));
        trace.entries.push(send(4, "someone-else", 1_000_000));
        look(&mut traffic, &plan, &trace, 1001, 1.0);
        let pulses = traffic.pulses(1.2, false);
        assert_eq!(pulses.len(), 1);
        assert_eq!((&pulses[0].event.from, &pulses[0].event.to), (&End::Chief, &End::Task("t2".into())));
        assert_eq!((pulses[0].event.who.as_str(), pulses[0].event.text.as_str()), ("Chief", "Rebase on main"));
        assert!(!pulses[0].event.agent);
    }

    #[test]
    fn notes_mark_rows_pins_mark_the_chief_and_held_messages_wait_at_the_node() {
        let mut traffic = Traffic::default();
        let mut plan = plan();
        let trace = TraceData::default();
        look(&mut traffic, &plan, &trace, 1000, 0.0);
        plan.notebook.push(note(1, NoteKind::Fact, "t1", None, 1000));
        let mut held = note(2, NoteKind::Message, "t1", Some("t3"), 1000);
        held.relay = Some(atomics_core::notebook::Relay {
            state: RelayState::Pending,
            expires_at: 2000,
            decided_by: None,
            decided_at: None,
            note: None,
        });
        plan.notebook.push(held);
        plan.notebook.push(note(3, NoteKind::Broadcast, "t2", None, 1000));
        look(&mut traffic, &plan, &trace, 1000, 1.0);
        let marks = traffic.marks(1.0, false);
        assert!(
            marks.iter().any(|(e, _)| e.kind == Kind::Note && e.to == End::Task("t1".into())),
            "a note marks its row"
        );
        // A broadcast goes to every other row; the held message waits at the chief's node, there for good.
        let pulses = traffic.pulses(2.0, false);
        let to: Vec<_> = pulses.iter().map(|p| (p.event.to.clone(), p.event.held)).collect();
        assert!(to.contains(&(End::Chief, Held::Waiting)));
        assert!(to.contains(&(End::Task("t1".into()), Held::No)) && to.contains(&(End::Task("t3".into()), Held::No)));
        let waiting = traffic.pulses(500.0, false);
        assert_eq!(waiting.len(), 1, "only the held one is left");
        assert_eq!(waiting[0].progress, 1.0);
        assert!(traffic.active(500.0));
        // Approved, it goes on from the node to its recipient.
        plan.notebook[1].relay.as_mut().unwrap().state = RelayState::Approved;
        plan.notebook[0].pinned = true;
        look(&mut traffic, &plan, &trace, 1500, 501.0);
        let pulses = traffic.pulses(501.5, false);
        assert_eq!(pulses.len(), 1);
        assert_eq!((&pulses[0].event.from, &pulses[0].event.to), (&End::Chief, &End::Task("t3".into())));
        assert!(traffic.marks(501.5, false).iter().any(|(e, _)| e.kind == Kind::Pinned && e.to == End::Chief));
    }

    #[test]
    fn a_denied_message_stops_at_the_node() {
        let mut traffic = Traffic::default();
        let mut plan = plan();
        let trace = TraceData::default();
        look(&mut traffic, &plan, &trace, 1000, 0.0);
        let mut held = note(5, NoteKind::Message, "t1", Some("t2"), 1000);
        held.relay = Some(atomics_core::notebook::Relay {
            state: RelayState::Pending,
            expires_at: 2000,
            decided_by: None,
            decided_at: None,
            note: None,
        });
        plan.notebook.push(held);
        look(&mut traffic, &plan, &trace, 1000, 1.0);
        plan.notebook[0].relay.as_mut().unwrap().state = RelayState::Denied;
        look(&mut traffic, &plan, &trace, 1001, 10.0);
        assert!(traffic.pulses(10.1, false).is_empty(), "nothing goes on");
        let marks = traffic.marks(10.1, false);
        assert_eq!(marks.len(), 1);
        assert_eq!((marks[0].0.held, &marks[0].0.to), (Held::Stopped, &End::Chief));
        assert!(!traffic.active(10.0 + MARK + 1.0));
    }

    #[test]
    fn reduced_motion_lands_everything_at_once_as_marks() {
        let mut traffic = Traffic::default();
        let mut plan = plan();
        let trace = TraceData::default();
        look(&mut traffic, &plan, &trace, 1000, 0.0);
        plan.tasks[1].report = Some(report(1000, "question"));
        look(&mut traffic, &plan, &trace, 1000, 5.0);
        assert!(traffic.pulses(5.1, true).is_empty());
        assert!(!traffic.moving(5.1, true));
        let marks = traffic.marks(5.1, true);
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].0.from, End::Task("t2".into()));
        assert!(traffic.marks(5.0 + MARK + 0.1, true).is_empty());
    }

    #[test]
    fn at_most_a_few_pulses_fly_at_once_and_events_stay_bounded() {
        let mut traffic = Traffic::default();
        let mut plan = plan();
        let mut trace = TraceData::default();
        look(&mut traffic, &plan, &trace, 1000, 0.0);
        for n in 0..40 {
            trace.entries.push(send(100 + n, "s-t1", 1_000_000));
        }
        plan.tasks[2].report = Some(report(1000, "done"));
        look(&mut traffic, &plan, &trace, 1000, 1.0);
        assert!(traffic.pulses(1.1, false).len() <= MAX_PULSES);
        assert!(traffic.events.len() <= MAX_EVENTS);
    }

    #[test]
    fn another_plan_starts_over_without_playing_it() {
        let mut traffic = Traffic::default();
        let mut plan = plan();
        let trace = TraceData::default();
        look(&mut traffic, &plan, &trace, 1000, 0.0);
        plan.tasks[0].report = Some(report(1000, "done"));
        look(&mut traffic, &plan, &trace, 1000, 1.0);
        assert_eq!(traffic.events.len(), 1);
        let mut other = plan.clone();
        other.id = "p2".into();
        other.tasks[1].report = Some(report(1000, "done"));
        look(&mut traffic, &other, &trace, 1000, 2.0);
        assert!(traffic.events.is_empty());
    }
}
