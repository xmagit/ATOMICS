//! A board's run log, for its History: what passed between the chief and its agents and between the agents, newest
//! first; how much passed between each pair, both ways; and a line that sums it up. It is made only from what the
//! host records:
//!
//! - the chief's calls in its trace ([`atomics_core::trace`]): `atomics_send` (a message, its text cut to 200
//!   characters there), `atomics_send_review`, `atomics_approve` (its answer to an agent's permission request) and
//!   `atomics_plan_update`, each tied to a task by the task id or the session it names. The trace masks a session id
//!   that looks like a credential, so a message finds its task by its text in the tasks' notes instead, and a review
//!   or an approval the host filed under the board, whose agent it cannot tell, is to "an agent";
//! - each task's notes ([`Task::notes`](atomics_core::plan::Task::notes)): every plain message an agent of the board
//!   was sent, whole, but without its time or who sent it. A message the trace has lends it its whole text; one it
//!   has not is a message from the chief or the user, listed last, its time not kept;
//! - each task's dispatch time, and its latest report: the host keeps one report a task, not every one;
//! - the plan's notebook: notes proposed and pinned, agents' messages and broadcasts, and what the relay did with
//!   the ones it held (delivered, not delivered or expired), by whom and when.
//!
//! Subagents come from the sessions' activity, names and counts only ([`subagents`]). The host keeps no time for
//! them, so they are counted beside the pairs rather than placed in the timeline.

use std::collections::{BTreeMap, HashMap};

use atomics_core::notebook::{Kind as NoteKind, RelayState};
use atomics_core::plan::{Decider, Plan, ReviewRound, Verdict};
use atomics_core::projection::ActivityInfo;
use atomics_core::trace::{TraceEntry, TracePhase, TraceState};
use serde_json::Value;

/// How a task's notes begin a message to its agent.
pub const MESSAGE_NOTE: &str = "Message to agent:\n";

/// Who sent something, or got it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Party {
    Chief,
    Task(String),
    /// The user, deciding a held message.
    You,
    /// Every other task: a broadcast.
    All,
    /// The chief or the user: a message the host kept without its sender.
    ChiefOrYou,
    /// One of the board's agents the trace does not say.
    SomeAgent,
}

impl Party {
    /// Where it goes in a pair and in a list: the chief first, then the user, then the tasks in the plan's order.
    fn rank(&self, plan: &Plan) -> (u8, usize) {
        match self {
            Party::Chief => (0, 0),
            Party::ChiefOrYou => (1, 0),
            Party::You => (1, 1),
            Party::Task(id) => (2, plan.tasks.iter().position(|t| &t.id == id).unwrap_or(usize::MAX)),
            Party::SomeAgent => (2, usize::MAX),
            Party::All => (3, 0),
        }
    }

    /// Its name, short: "Chief", "t1", "You", "All", "Chief or you".
    pub fn short(&self) -> &str {
        match self {
            Party::Chief => "Chief",
            Party::Task(id) => id,
            Party::You => "You",
            Party::All => "All",
            Party::ChiefOrYou => "Chief or you",
            Party::SomeAgent => "An agent",
        }
    }
}

/// Where an agent's message stands with the relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relay {
    /// Delivered without review.
    Direct,
    /// Waits for the chief's or the user's review.
    Held,
    Delivered,
    Denied,
    Expired,
}

/// What passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum What {
    /// A message to a task's agent: the chief's, or one kept without its sender ([`Party::ChiefOrYou`]).
    Message,
    /// The chief sent an agent its review comments.
    Review,
    /// A task's report, by its status: done, blocked, question or progress.
    Report(String),
    /// A note an agent proposed for the notebook, pinned once it was kept.
    Note { pinned: bool },
    /// An agent's message to another, or a broadcast to all, and where it stands with the relay.
    AgentMessage(Relay),
    /// What became of a message the relay held, between the same two as the message, and who decided it: the chief
    /// or the user; nobody where it expired.
    Decided(Relay, Option<Party>),
    /// The chief's answer to an agent's permission request: allow, allow_always or deny.
    Approval(String),
    /// The task was dispatched; the text is its brief.
    Dispatch,
    /// The chief changed the task on the board: a status, or a note the user sees.
    Update,
    /// A round of review: its number and, once decided, its verdict; the final review's own where `final_review`.
    Round { round: u32, verdict: Option<Verdict>, final_review: bool, blocking: Option<u32> },
}

/// What a round's verdict says, short: "accepted", "changes requested", "changes requested, 2 blocking".
pub fn verdict_words(verdict: Option<Verdict>, blocking: Option<u32>, final_review: bool) -> String {
    match verdict {
        None if final_review => "reviewing".into(),
        None => "in review".into(),
        Some(Verdict::Accepted) => "accepted".into(),
        Some(Verdict::ChangesRequested) => "changes requested".into(),
        Some(Verdict::Blocked) => "blocked".into(),
        Some(Verdict::Approve) => "approve".into(),
        Some(Verdict::Changes) => match blocking {
            Some(n) if n > 0 => format!("changes requested, {n} blocking"),
            _ => "changes requested".into(),
        },
    }
}

/// What a pair's tally counts, in the order a breakdown lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Count {
    Dispatches,
    Messages,
    Reviews,
    Approvals,
    Reports,
    Notes,
}

impl Count {
    /// "3 messages", "1 report".
    pub fn words(self, n: usize) -> String {
        let (one, many) = match self {
            Count::Dispatches => ("dispatch", "dispatches"),
            Count::Messages => ("message", "messages"),
            Count::Reviews => ("review", "reviews"),
            Count::Approvals => ("approval", "approvals"),
            Count::Reports => ("report", "reports"),
            Count::Notes => ("note", "notes"),
        };
        format!("{n} {}", if n == 1 { one } else { many })
    }
}

impl What {
    /// The kind, in a word or two, as the timeline says it.
    pub fn word(&self, to: &Party) -> String {
        match self {
            What::Message => "Message".into(),
            What::Review => "Review".into(),
            What::Report(status) => format!("Report · {status}"),
            What::Note { pinned: false } => "Note proposed".into(),
            What::Note { pinned: true } => "Note pinned".into(),
            What::AgentMessage(relay) => {
                let what = if *to == Party::All { "Broadcast" } else { "Message" };
                match relay {
                    Relay::Held => format!("{what} · held"),
                    _ => what.into(),
                }
            }
            What::Decided(relay, by) => {
                let what = match relay {
                    Relay::Delivered => "Delivered",
                    Relay::Denied => "Not delivered",
                    _ => "Expired",
                };
                match by {
                    Some(Party::You) => format!("{what} · you"),
                    Some(_) => format!("{what} · chief"),
                    None => what.into(),
                }
            }
            What::Approval(decision) => match decision.as_str() {
                "deny" => "Denied a request".into(),
                "allow_always" => "Allowed always".into(),
                _ => "Allowed a request".into(),
            },
            What::Dispatch => "Dispatched".into(),
            What::Update => "Board update".into(),
            What::Round { round, verdict, final_review, blocking } => {
                let what = if *final_review { "Final review round" } else { "Review round" };
                format!("{what} {round} · {}", verdict_words(*verdict, *blocking, *final_review))
            }
        }
    }

    /// What it adds to between its two parties; nothing for what only changes the board or decides a message.
    pub fn count(&self) -> Option<Count> {
        match self {
            What::Message | What::AgentMessage(_) => Some(Count::Messages),
            What::Review => Some(Count::Reviews),
            What::Report(_) => Some(Count::Reports),
            What::Note { .. } => Some(Count::Notes),
            What::Approval(_) => Some(Count::Approvals),
            What::Dispatch => Some(Count::Dispatches),
            What::Decided(..) | What::Update | What::Round { .. } => None,
        }
    }
}

/// One thing that passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Stable across reads, for what is open.
    pub key: String,
    /// The host's clock, in seconds; `None` where the host keeps no time for it.
    pub at: Option<u64>,
    pub what: What,
    pub from: Party,
    pub to: Party,
    /// What it said, whole as far as the host keeps it.
    pub text: String,
    /// The text is an agent's: data, not instructions.
    pub agent: bool,
    /// Why a held message was or was not delivered, as whoever decided it wrote.
    pub reason: Option<String>,
}

/// How much went one way between two parties, and of what.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tally {
    pub total: usize,
    pub by: BTreeMap<Count, usize>,
}

impl Tally {
    fn add(&mut self, count: Count) {
        self.total += 1;
        *self.by.entry(count).or_default() += 1;
    }

    /// "2 messages, 1 review".
    pub fn words(&self) -> String {
        self.by.iter().map(|(count, n)| count.words(*n)).collect::<Vec<_>>().join(", ")
    }
}

/// Two parties that talked: `a` is the one first in the plan's order (the chief before any task).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pair {
    pub a: Party,
    pub b: Party,
    /// From `a` to `b`.
    pub ab: Tally,
    /// From `b` to `a`.
    pub ba: Tally,
}

impl Pair {
    pub fn total(&self) -> usize {
        self.ab.total + self.ba.total
    }

    /// Whether an entry passed between these two, either way.
    pub fn holds(&self, entry: &Entry) -> bool {
        (entry.from == self.a && entry.to == self.b) || (entry.from == self.b && entry.to == self.a)
    }
}

/// The line over the History.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub messages: usize,
    pub reports: usize,
    pub notes: usize,
    pub approvals: usize,
    /// Review rounds on every task, the final review's included.
    pub rounds: u32,
    /// The task that took the most rounds, and how many.
    pub most: Option<(String, u32)>,
    /// The first and the last time anything passed, by the host's clock.
    pub span: Option<(u64, u64)>,
}

impl Summary {
    /// "12 messages · 3 reports · 9 review rounds, t2 took 5 · 1 note over 42 min"; what is none is left out.
    pub fn line(&self) -> String {
        let words = |(count, n): (Count, usize)| (n > 0).then(|| count.words(n));
        let rounds = (self.rounds > 0).then(|| {
            let plural = if self.rounds == 1 { "" } else { "s" };
            match &self.most {
                Some((task, most)) if *most > 1 => format!("{} review round{plural}, {task} took {most}", self.rounds),
                _ => format!("{} review round{plural}", self.rounds),
            }
        });
        let parts: Vec<String> =
            [words((Count::Messages, self.messages)), words((Count::Reports, self.reports)), rounds]
                .into_iter()
                .chain([words((Count::Notes, self.notes)), words((Count::Approvals, self.approvals))])
                .flatten()
                .collect();
        if parts.is_empty() {
            return "Nothing has passed between the chief and its agents yet".into();
        }
        let span = match self.span {
            Some((first, last)) if last > first + 59 => format!(" over {}", duration(last - first)),
            Some(_) => " within a minute".into(),
            None => String::new(),
        };
        format!("{}{span}", parts.join(" · "))
    }
}

/// A span of time, short: "45 s", "12 min", "1 h 12 min", "3 d 4 h".
pub fn duration(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs} s"),
        60..3600 => format!("{} min", secs / 60),
        3600..86400 if secs % 3600 / 60 == 0 => format!("{} h", secs / 3600),
        3600..86400 => format!("{} h {} min", secs / 3600, secs % 3600 / 60),
        _ if secs % 86400 / 3600 == 0 => format!("{} d", secs / 86400),
        _ => format!("{} d {} h", secs / 86400, secs % 86400 / 3600),
    }
}

/// A board's run log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Log {
    /// Newest first; what has no time after the rest, as it was kept.
    pub entries: Vec<Entry>,
    /// The pairs that talked, the most first.
    pub pairs: Vec<Pair>,
    /// Each task's review rounds, in the plan's order with the final review last.
    pub reviews: Vec<Rounds>,
    pub summary: Summary,
}

/// The run log of `plan`, from the chief's `calls` (the live trace's and any the host kept for the board; the same
/// call twice counts once, its completion over its start).
pub fn build<'a>(plan: &Plan, calls: impl IntoIterator<Item = &'a TraceEntry>) -> Log {
    let sessions: HashMap<&str, &str> =
        plan.tasks.iter().filter_map(|t| Some((t.session.as_deref()?, t.id.as_str()))).collect();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut chief: Vec<(&TraceEntry, Entry)> = Vec::new();
    for call in calls {
        let Some(entry) = from_call(call, plan, &sessions) else { continue };
        match seen.get(&entry.key) {
            Some(&k) if chief[k].0.phase == TracePhase::Post || call.phase == TracePhase::Pre => {}
            Some(&k) => chief[k] = (call, entry),
            None => {
                seen.insert(entry.key.clone(), chief.len());
                chief.push((call, entry));
            }
        }
    }
    let mut chief: Vec<(bool, Entry)> = chief
        .into_iter()
        .filter(|(call, _)| call.state != TraceState::Error)
        .map(|(call, entry)| (call.plan_id.as_deref() == Some(plan.id.as_str()), entry))
        .collect();
    chief.sort_by_key(|(_, entry)| entry.at);
    // The chief's messages lend their time to the whole text the tasks' notes keep, and one whose session the trace
    // masked finds its task by that text. The newest calls match the newest notes: the trace keeps the last calls, the
    // notes every message.
    let mut notes: Vec<(Vec<&str>, Vec<bool>, usize)> = plan
        .tasks
        .iter()
        .map(|task| {
            let notes: Vec<&str> = task.notes.iter().filter_map(|note| note.strip_prefix(MESSAGE_NOTE)).collect();
            let count = notes.len();
            (notes, vec![false; count], count)
        })
        .collect();
    for (_, entry) in chief.iter_mut().rev().filter(|(_, e)| e.what == What::Message && !e.text.is_empty()) {
        for (task, (notes, used, before)) in plan.tasks.iter().zip(&mut notes) {
            if entry.to != Party::SomeAgent && entry.to != Party::Task(task.id.clone()) {
                continue;
            }
            if let Some(k) = (0..*before).rev().find(|&k| shows(notes[k], &entry.text)) {
                entry.text = notes[k].to_string();
                entry.to = Party::Task(task.id.clone());
                used[k] = true;
                *before = k;
                break;
            }
        }
    }
    // A call whose agent is still unknown is the board's only where the host filed it under the board.
    let mut entries: Vec<Entry> =
        chief.into_iter().filter(|(ours, e)| *ours || e.to != Party::SomeAgent).map(|(_, entry)| entry).collect();
    let mut earlier = Vec::new();
    for (task, (notes, used, _)) in plan.tasks.iter().zip(&notes) {
        let to = Party::Task(task.id.clone());
        for (k, note) in notes.iter().enumerate().filter(|(k, _)| !used[*k]) {
            earlier.push(Entry {
                key: format!("earlier:{}:{k}", task.id),
                at: None,
                what: What::Message,
                from: Party::ChiefOrYou,
                to: to.clone(),
                text: note.to_string(),
                agent: false,
                reason: None,
            });
        }
        if let Some(at) = task.dispatched_at {
            entries.push(Entry {
                key: format!("dispatch:{}", task.id),
                at: Some(at),
                what: What::Dispatch,
                from: Party::Chief,
                to: to.clone(),
                text: task.brief.clone(),
                agent: false,
                reason: None,
            });
        }
        if let Some(report) = &task.report {
            entries.push(Entry {
                key: format!("report:{}", task.id),
                at: Some(report.at),
                what: What::Report(report.status.clone()),
                from: to.clone(),
                to: Party::Chief,
                text: report.summary.clone(),
                agent: true,
                reason: None,
            });
        }
    }
    for note in &plan.notebook {
        entries.extend(from_note(note));
    }
    for task in &plan.tasks {
        entries.extend(task.reviews.iter().map(|round| from_round(task, round)));
    }
    // Newest first; at the same second, the one added later first.
    let mut timed: Vec<(usize, Entry)> = entries.into_iter().enumerate().collect();
    timed.sort_by(|(i, a), (j, b)| b.at.cmp(&a.at).then(j.cmp(i)));
    let mut entries: Vec<Entry> = timed.into_iter().map(|(_, entry)| entry).collect();
    entries.extend(earlier);
    let pairs = pairs(plan, &entries);
    let mut summary = Summary::default();
    for entry in &entries {
        match entry.what.count() {
            Some(Count::Messages | Count::Reviews) => summary.messages += 1,
            Some(Count::Reports) => summary.reports += 1,
            Some(Count::Notes) => summary.notes += 1,
            Some(Count::Approvals) => summary.approvals += 1,
            Some(Count::Dispatches) | None => {}
        }
    }
    let times = entries.iter().filter_map(|entry| entry.at);
    summary.span = times.clone().min().zip(times.max());
    let reviews = rounds(plan);
    summary.rounds = reviews.iter().map(Rounds::count).sum();
    summary.most = reviews
        .iter()
        .max_by_key(|task| (task.count(), std::cmp::Reverse(task.final_review)))
        .map(|task| (task.name().to_string(), task.count()));
    Log { entries, pairs, summary, reviews }
}

/// A review round as the timeline lists it: decided, from whoever decided it to the task; still open, from the task.
fn from_round(task: &atomics_core::plan::Task, round: &ReviewRound) -> Entry {
    let final_review = task.id == atomics_core::plan::REVIEW_TASK;
    let what = What::Round { round: round.round, verdict: round.verdict, final_review, blocking: round.blocking };
    let own = Party::Task(task.id.clone());
    let key = format!("round:{}:{}", task.id, round.round);
    let Some(closed) = round.closed.filter(|_| round.verdict.is_some()) else {
        let (at, text) = (Some(round.opened), round.report.clone());
        return Entry { key, at, what, from: own, to: Party::Chief, text, agent: true, reason: None };
    };
    let by = match round.by {
        Some(Decider::Chief) => Party::Chief,
        Some(Decider::User) => Party::You,
        Some(Decider::Reviewer | Decider::Agent) => own.clone(),
        None => Party::ChiefOrYou,
    };
    let agent_said = matches!(round.by, Some(Decider::Reviewer | Decider::Agent));
    let (text, agent) = match round.said.is_empty() {
        true => (round.report.clone(), true),
        false => (round.said.clone(), agent_said),
    };
    let (from, to) = if by == own { (own, Party::Chief) } else { (by, own) };
    Entry { key, at: Some(closed), what, from, to, text, agent, reason: None }
}

/// One task's review rounds, for the History's Reviews card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rounds {
    pub task: String,
    pub title: String,
    pub final_review: bool,
    /// Oldest first, as the host keeps them.
    pub rounds: Vec<ReviewRound>,
}

impl Rounds {
    /// How many rounds it took so far, counting those the host no longer keeps.
    pub fn count(&self) -> u32 {
        self.rounds.last().map_or(0, |round| round.round)
    }

    /// Its name as the card says it.
    pub fn name(&self) -> &str {
        if self.final_review { "Final review" } else { &self.task }
    }

    /// Where it stands, leading with the count: "accepted after 5 rounds", "round 3, changes requested".
    pub fn words(&self) -> String {
        let Some(last) = self.rounds.last() else { return String::new() };
        let n = last.round;
        match last.verdict {
            Some(Verdict::Accepted) => format!("accepted after {n} round{}", if n == 1 { "" } else { "s" }),
            Some(Verdict::Approve) => format!("approved after {n} round{}", if n == 1 { "" } else { "s" }),
            verdict => format!("round {n}, {}", verdict_words(verdict, last.blocking, self.final_review)),
        }
    }

    /// How long from its first round until it was accepted or approved, where it was.
    pub fn took(&self) -> Option<u64> {
        let (first, last) = (self.rounds.first()?, self.rounds.last()?);
        matches!(last.verdict, Some(Verdict::Accepted | Verdict::Approve))
            .then(|| last.closed.unwrap_or(last.opened).saturating_sub(first.opened))
    }
}

/// The review rounds of each of `plan`'s tasks that had any, in the plan's order with the final review last.
pub fn rounds(plan: &Plan) -> Vec<Rounds> {
    let mut all: Vec<Rounds> = plan
        .tasks
        .iter()
        .filter(|task| !task.reviews.is_empty())
        .map(|task| Rounds {
            task: task.id.clone(),
            title: task.title.clone(),
            final_review: task.id == atomics_core::plan::REVIEW_TASK,
            rounds: task.reviews.clone(),
        })
        .collect();
    all.sort_by_key(|task| task.final_review);
    all
}

/// Whether the trace's `shown` text is the start of `whole`, but for what the trace masked as `***`.
pub fn shows(whole: &str, shown: &str) -> bool {
    let mut parts = shown.split("***");
    let Some(mut rest) = whole.strip_prefix(parts.next().unwrap_or_default()) else { return false };
    for part in parts {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    true
}

/// One of the chief's calls, as an entry of `plan`'s log, where it is one of the board's.
fn from_call(call: &TraceEntry, plan: &Plan, sessions: &HashMap<&str, &str>) -> Option<Entry> {
    if call.turn.is_some() || call.server != "atomics" {
        return None;
    }
    let args: Value = serde_json::from_str(&call.summary).ok()?;
    // A session the board does not hold is another board's; one the trace masked could be any agent's.
    let agent = || match args["session"].as_str() {
        Some(session) if sessions.contains_key(session) => Some(Party::Task(sessions[session].to_string())),
        Some(session) if session.contains("***") => Some(Party::SomeAgent),
        _ => None,
    };
    let text = |key: &str| args[key].as_str().unwrap_or_default().trim().to_string();
    let (what, to, text) = match call.tool.as_str() {
        "atomics_send" if args["raw"] != Value::Bool(true) => (What::Message, agent()?, text("text")),
        "atomics_send_review" => (What::Review, agent()?, "Review comments on its diff".into()),
        "atomics_approve" => (What::Approval(text("decision")), agent()?, text("message")),
        "atomics_plan_update" => {
            let ours =
                args["plan"].as_str().map_or(call.plan_id.as_deref() == Some(plan.id.as_str()), |id| id == plan.id);
            let task = args["id"].as_str().filter(|id| ours && plan.tasks.iter().any(|t| t.id == *id))?;
            let status = args["status"].as_str().map(|status| format!("Status: {status}"));
            let words: Vec<String> = status.into_iter().chain(Some(text("note")).filter(|n| !n.is_empty())).collect();
            (What::Update, Party::Task(task.to_string()), words.join("\n"))
        }
        _ => return None,
    };
    Some(Entry {
        key: format!("call:{}:{}", call.session_id, call.tool_use_id.clone().unwrap_or_else(|| call.seq.to_string())),
        at: Some(call.at / 1000),
        what,
        from: Party::Chief,
        to,
        text,
        agent: false,
        reason: None,
    })
}

/// A notebook entry: a note, or an agent's message with what the relay did with it.
fn from_note(note: &atomics_core::plan::NotebookEntry) -> Vec<Entry> {
    let text = if note.hidden { "(hidden message)".to_string() } else { note.text.clone() };
    // The chief's own broadcasts are signed "chief".
    let chief = note.task == "chief" && note.agent == "chief";
    let from = if chief { Party::Chief } else { Party::Task(note.task.clone()) };
    let key = format!("note:{}", note.id);
    if note.kind == NoteKind::Fact {
        let what = What::Note { pinned: note.pinned };
        return vec![Entry { key, at: Some(note.at), what, from, to: Party::Chief, text, agent: true, reason: None }];
    }
    let to = match (&note.kind, note.to.as_deref()) {
        (NoteKind::Message, Some(to)) if to != "*" => Party::Task(to.to_string()),
        _ => Party::All,
    };
    let relay = match note.relay.as_ref().map(|r| r.state) {
        None => Relay::Direct,
        Some(RelayState::Pending) => Relay::Held,
        Some(RelayState::Approved) => Relay::Delivered,
        Some(RelayState::Denied) => Relay::Denied,
        Some(RelayState::Expired) => Relay::Expired,
    };
    let mut entries = vec![Entry {
        key: key.clone(),
        at: Some(note.at),
        what: What::AgentMessage(relay),
        from: from.clone(),
        to: to.clone(),
        text: text.clone(),
        agent: true,
        reason: None,
    }];
    if let Some(held) =
        note.relay.as_ref().filter(|_| matches!(relay, Relay::Delivered | Relay::Denied | Relay::Expired))
    {
        let by = match (relay, held.decided_by.as_deref()) {
            (Relay::Expired, _) => None,
            (_, Some("user")) => Some(Party::You),
            _ => Some(Party::Chief),
        };
        entries.push(Entry {
            key: format!("{key}:decided"),
            at: Some(held.decided_at.unwrap_or(held.expires_at)),
            what: What::Decided(relay, by),
            from,
            to,
            text,
            agent: true,
            reason: held.note.clone().filter(|why| !why.trim().is_empty()),
        });
    }
    entries
}

/// How much passed between each two parties, the most first.
fn pairs(plan: &Plan, entries: &[Entry]) -> Vec<Pair> {
    let mut pairs: Vec<Pair> = Vec::new();
    for entry in entries {
        let Some(count) = entry.what.count() else { continue };
        if entry.from == entry.to {
            continue;
        }
        let forward = entry.from.rank(plan) <= entry.to.rank(plan);
        let (a, b) = if forward { (&entry.from, &entry.to) } else { (&entry.to, &entry.from) };
        let pair = match pairs.iter().position(|pair| pair.a == *a && pair.b == *b) {
            Some(k) => &mut pairs[k],
            None => {
                pairs.push(Pair { a: a.clone(), b: b.clone(), ab: Tally::default(), ba: Tally::default() });
                pairs.last_mut().expect("just pushed")
            }
        };
        if forward { pair.ab.add(count) } else { pair.ba.add(count) }
    }
    pairs.sort_by(|x, y| {
        y.total().cmp(&x.total()).then((x.a.rank(plan), x.b.rank(plan)).cmp(&(y.a.rank(plan), y.b.rank(plan))))
    });
    pairs
}

/// A task's subagents, by kind: how many it started, and how many run now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subagents {
    pub task: String,
    /// Each kind with how many, in the order they first started.
    pub kinds: Vec<(String, usize)>,
    pub running: usize,
}

impl Subagents {
    pub fn total(&self) -> usize {
        self.kinds.iter().map(|(_, n)| n).sum()
    }
}

/// The subagents of each of `plan`'s tasks whose session the host reads, from `activity` by session id.
pub fn subagents<'a>(plan: &Plan, activity: impl Fn(&str) -> Option<&'a ActivityInfo>) -> Vec<Subagents> {
    plan.tasks
        .iter()
        .filter_map(|task| {
            let info = activity(task.session.as_deref()?)?;
            let mut kinds: Vec<(String, usize)> = Vec::new();
            for thread in &info.threads {
                match kinds.iter_mut().find(|(kind, _)| *kind == thread.kind) {
                    Some((_, n)) => *n += 1,
                    None => kinds.push((thread.kind.clone(), 1)),
                }
            }
            let running = info.threads.iter().filter(|thread| thread.running).count();
            (!kinds.is_empty()).then(|| Subagents { task: task.id.clone(), kinds, running })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomics_core::notebook::Relay as Held;
    use atomics_core::plan::{NotebookEntry, Report, Task, TaskStatus};
    use atomics_core::trace::TraceOp;

    fn plan() -> Plan {
        let task = |id: &str| Task {
            id: id.into(),
            title: format!("Task {id}"),
            brief: format!("Do {id}"),
            status: TaskStatus::Running,
            session: Some(format!("s-{id}")),
            ..Task::default()
        };
        Plan { id: "p1".into(), tasks: vec![task("t1"), task("t2"), task("t3")], ..Plan::default() }
    }

    fn call(seq: u64, tool: &str, at_ms: u64, args: Value) -> TraceEntry {
        TraceEntry {
            chat_id: None,
            session_id: "chief".into(),
            seq,
            at: at_ms,
            phase: TracePhase::Post,
            tool_use_id: Some(format!("u{seq}")),
            server: "atomics".into(),
            tool: tool.into(),
            op: TraceOp::Other,
            direct: false,
            plan_id: None,
            summary: args.to_string(),
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

    fn note(id: u64, kind: NoteKind, task: &str, to: Option<&str>, at: u64) -> NotebookEntry {
        NotebookEntry {
            id,
            task: task.into(),
            agent: "worker".into(),
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

    fn held(state: RelayState, by: Option<&str>, at: Option<u64>) -> Option<Held> {
        Some(Held { state, expires_at: 5000, decided_by: by.map(str::to_string), decided_at: at, note: None })
    }

    /// A board where the chief dispatched, messaged, reviewed and approved, tasks reported and agents talked.
    fn board() -> (Plan, Vec<TraceEntry>) {
        let mut plan = plan();
        let long = format!("Rebase on main{}", " and run the tests".repeat(20));
        plan.tasks[0].dispatched_at = Some(1000);
        plan.tasks[1].dispatched_at = Some(1001);
        plan.tasks[0].notes = vec![
            format!("{MESSAGE_NOTE}Look at the reader first"),
            "Could not start: a warning that is no message".into(),
            format!("{MESSAGE_NOTE}{long}"),
        ];
        plan.tasks[0].report =
            Some(Report { status: "done".into(), summary: "Split it\nmore".into(), at: 1100, checks: Vec::new() });
        plan.tasks[1].report =
            Some(Report { status: "question".into(), summary: "Which API?".into(), at: 1050, checks: Vec::new() });
        let mut fact = note(1, NoteKind::Fact, "t2", None, 1040);
        fact.pinned = true;
        let mut to_t3 = note(2, NoteKind::Message, "t1", Some("t3"), 1060);
        to_t3.relay = held(RelayState::Approved, Some("user"), Some(1070));
        let mut denied = note(3, NoteKind::Message, "t2", Some("t1"), 1080);
        denied.relay = held(RelayState::Denied, Some("chief"), Some(1090));
        let waiting = {
            let mut n = note(4, NoteKind::Message, "t3", Some("t1"), 1095);
            n.relay = held(RelayState::Pending, None, None);
            n
        };
        plan.notebook = vec![fact, to_t3, denied, waiting, note(5, NoteKind::Broadcast, "t3", None, 1097)];
        let short: String = long.chars().take(200).collect();
        let mut failed = call(9, "atomics_send", 1_030_000, serde_json::json!({"session": "s-t2", "text": "lost"}));
        failed.state = TraceState::Error;
        let mut turn = call(10, "atomics_send", 1_031_000, serde_json::json!({"session": "s-t2", "text": "x"}));
        turn.turn = Some(atomics_core::trace::Turn { role: atomics_core::trace::TurnRole::Chief, text: "x".into() });
        let trace = vec![
            call(1, "atomics_send", 1_020_000, serde_json::json!({"session": "s-t1", "text": short})),
            call(2, "atomics_send_review", 1_025_000, serde_json::json!({"session": "s-t2"})),
            call(3, "atomics_approve", 1_026_000, serde_json::json!({"session": "s-t3", "decision": "deny"})),
            call(4, "atomics_plan_update", 1_110_000, serde_json::json!({"plan": "p1", "id": "t1", "status": "done"})),
            // Another board's, an agent outside it, keystrokes, a failed call and a turn: none of the board's log.
            call(5, "atomics_plan_update", 1_111_000, serde_json::json!({"plan": "p2", "id": "t1", "status": "done"})),
            call(6, "atomics_send", 1_027_000, serde_json::json!({"session": "elsewhere", "text": "hi"})),
            call(7, "atomics_send", 1_028_000, serde_json::json!({"session": "s-t1", "text": "\\r", "raw": true})),
            call(8, "atomics_list", 1_029_000, serde_json::json!({})),
            failed,
            turn,
        ];
        (plan, trace)
    }

    #[test]
    fn the_log_reads_newest_first_and_what_has_no_time_comes_last() {
        let (plan, trace) = board();
        let log = build(&plan, &trace);
        let seen: Vec<(Option<u64>, String, &str, &str)> =
            log.entries.iter().map(|e| (e.at, e.what.word(&e.to), e.from.short(), e.to.short())).collect();
        let at = |n: u64, what: &str, from: &'static str, to: &'static str| (Some(n), what.to_string(), from, to);
        assert_eq!(
            seen,
            [
                at(1110, "Board update", "Chief", "t1"),
                at(1100, "Report · done", "t1", "Chief"),
                at(1097, "Broadcast", "t3", "All"),
                at(1095, "Message · held", "t3", "t1"),
                at(1090, "Not delivered · chief", "t2", "t1"),
                at(1080, "Message", "t2", "t1"),
                at(1070, "Delivered · you", "t1", "t3"),
                at(1060, "Message", "t1", "t3"),
                at(1050, "Report · question", "t2", "Chief"),
                at(1040, "Note pinned", "t2", "Chief"),
                at(1026, "Denied a request", "Chief", "t3"),
                at(1025, "Review", "Chief", "t2"),
                at(1020, "Message", "Chief", "t1"),
                at(1001, "Dispatched", "Chief", "t2"),
                at(1000, "Dispatched", "Chief", "t1"),
                (None, "Message".into(), "Chief or you", "t1"),
            ]
        );
        // The chief's call, cut in its trace, reads whole from the task's notes; the note it did not send has no time.
        let sent = log.entries.iter().find(|e| e.key == "call:chief:u1").unwrap();
        assert!(sent.text.starts_with("Rebase on main") && sent.text.chars().count() > 300, "{}", sent.text);
        assert!(!sent.agent);
        assert_eq!(log.entries.last().unwrap().text, "Look at the reader first");
        let update = &log.entries[0];
        assert_eq!(update.text, "Status: done");
        let report = &log.entries[1];
        assert!(report.agent && report.text == "Split it\nmore");
        assert_eq!(log.entries.iter().find(|e| e.what == What::Dispatch).unwrap().text, "Do t2");
    }

    #[test]
    fn pairs_count_both_ways_once_each_and_the_busiest_come_first() {
        let (plan, trace) = board();
        let log = build(&plan, &trace);
        let pairs: Vec<(&str, &str, usize, usize)> =
            log.pairs.iter().map(|p| (p.a.short(), p.b.short(), p.ab.total, p.ba.total)).collect();
        assert_eq!(
            pairs,
            [
                // Dispatched and reviewed, a question back and its note.
                ("Chief", "t2", 2, 2),
                // Dispatched and messaged, one report back.
                ("Chief", "t1", 2, 1),
                ("t1", "t3", 1, 1),
                // As many either way: the plan's order.
                ("Chief", "t3", 1, 0),
                ("Chief or you", "t1", 1, 0),
                ("t1", "t2", 0, 1),
                ("t3", "All", 1, 0),
            ]
        );
        let chief_t2 = log.pairs.iter().find(|p| p.b == Party::Task("t2".into())).unwrap();
        assert_eq!(chief_t2.ab.words(), "1 dispatch, 1 review");
        assert_eq!(chief_t2.ba.words(), "1 report, 1 note");
        // A decision on a held message is no second message, and what a pair holds is what passed either way, the
        // relay's decisions on it too.
        let t1_t3 = log.pairs.iter().find(|p| p.a == Party::Task("t1".into()) && p.b == Party::Task("t3".into()));
        let t1_t3 = t1_t3.unwrap();
        let held: Vec<&str> = log.entries.iter().filter(|e| t1_t3.holds(e)).map(|e| e.key.as_str()).collect();
        assert_eq!(held, ["note:4", "note:2:decided", "note:2"]);
        assert_eq!(
            log.summary,
            Summary { messages: 7, reports: 2, notes: 1, approvals: 1, span: Some((1000, 1110)), ..Summary::default() }
        );
        assert_eq!(log.summary.line(), "7 messages · 2 reports · 1 note · 1 approval over 1 min");
    }

    #[test]
    fn the_same_call_read_twice_counts_once_and_a_failed_completion_takes_it_back() {
        let plan = plan();
        let send = |seq, phase, state| {
            let mut entry = call(seq, "atomics_send", 1_000_000, serde_json::json!({"session": "s-t1", "text": "go"}));
            (entry.phase, entry.state) = (phase, state);
            entry
        };
        // Archived as it started and as it ended, and live: one message.
        let calls = [
            send(1, TracePhase::Pre, TraceState::Running),
            send(1, TracePhase::Post, TraceState::Ok),
            send(1, TracePhase::Post, TraceState::Ok),
        ];
        assert_eq!(build(&plan, &calls).summary.messages, 1);
        // Its completion failed: nothing passed, whichever was read first.
        let failed = [send(2, TracePhase::Post, TraceState::Error), send(2, TracePhase::Pre, TraceState::Running)];
        assert!(build(&plan, &failed).entries.is_empty());
        let failed = [send(2, TracePhase::Pre, TraceState::Running), send(2, TracePhase::Post, TraceState::Error)];
        assert!(build(&plan, &failed).entries.is_empty());
        // Still on its way, it counts.
        assert_eq!(build(&plan, &[send(3, TracePhase::Pre, TraceState::Running)]).summary.messages, 1);
    }

    #[test]
    fn the_chiefs_messages_take_the_newest_notes_that_match_and_the_rest_are_earlier() {
        let mut plan = plan();
        plan.tasks[0].notes = ["go on", "go on", "go on please"].map(|n| format!("{MESSAGE_NOTE}{n}")).to_vec();
        let send = |seq, at| call(seq, "atomics_send", at, serde_json::json!({"session": "s-t1", "text": "go on"}));
        let log = build(&plan, &[send(1, 1_000_000), send(2, 2_000_000)]);
        let texts: Vec<(&str, &str)> = log.entries.iter().map(|e| (e.from.short(), e.text.as_str())).collect();
        assert_eq!(texts, [("Chief", "go on please"), ("Chief", "go on"), ("Chief or you", "go on")]);
        assert_eq!(log.entries[2].key, "earlier:t1:0");
        assert_eq!(log.summary.span, Some((1000, 2000)));
    }

    #[test]
    fn a_masked_session_finds_its_task_by_the_messages_text_or_is_an_agent_of_the_board() {
        let mut plan = plan();
        let sha = "4f2a9c0d1e7b3a6f5c8d9e0a1b2c3d4e5f6a7b8c";
        plan.tasks[1].notes =
            vec![format!("{MESSAGE_NOTE}Cherry-pick {sha} onto the reader branch, then rerun the tests")];
        plan.tasks[2].notes = vec![format!("{MESSAGE_NOTE}Use an 8 KiB buffer")];
        let masked = |seq, tool: &str, plan_id: Option<&str>, args: Value| {
            let mut entry = call(seq, tool, 1_000_000 + seq * 1000, args);
            entry.plan_id = plan_id.map(str::to_string);
            entry
        };
        let calls = [
            // Its text, with the SHA masked as the trace masks it, is t2's note; another board's id does not matter.
            masked(
                1,
                "atomics_send",
                Some("p2"),
                serde_json::json!({"session": "***", "text": "Cherry-pick *** onto the reader"}),
            ),
            masked(2, "atomics_send", None, serde_json::json!({"session": "***", "text": "Use an 8 KiB buffer"})),
            // No note has its text: an agent of this board where the host filed it here, else none of the board's.
            masked(3, "atomics_send", Some("p1"), serde_json::json!({"session": "***", "text": "Stop for now"})),
            masked(4, "atomics_send", Some("p2"), serde_json::json!({"session": "***", "text": "Stop for now"})),
            masked(5, "atomics_approve", Some("p1"), serde_json::json!({"session": "***", "decision": "allow"})),
            masked(6, "atomics_approve", None, serde_json::json!({"session": "***", "decision": "deny"})),
        ];
        let log = build(&plan, &calls);
        let seen: Vec<(&str, String)> = log.entries.iter().map(|e| (e.to.short(), e.what.word(&e.to))).collect();
        let to = |who: &'static str, what: &str| (who, what.to_string());
        assert_eq!(
            seen,
            [to("An agent", "Allowed a request"), to("An agent", "Message"), to("t3", "Message"), to("t2", "Message")]
        );
        assert!(log.entries[3].text.contains(sha), "the whole text, from the note");
        assert!(shows("Rebase on main and test", "Rebase on main"));
        assert!(shows("a sk-123 b sk-456 c", "a *** b *** c"));
        assert!(!shows("Rebase on main", "Rebase on mainline"));
        assert!(!shows("a b", "a *** c"));
    }

    #[test]
    fn a_held_message_waits_expires_or_is_decided_by_whoever_decided_it() {
        let mut plan = plan();
        let mut expired = note(7, NoteKind::Message, "t1", Some("t2"), 100);
        expired.relay = held(RelayState::Expired, None, None);
        let mut hidden = note(8, NoteKind::Broadcast, "t2", Some("*"), 110);
        hidden.hidden = true;
        let mut from_chief = note(9, NoteKind::Broadcast, "chief", Some("*"), 90);
        from_chief.agent = "chief".into();
        plan.notebook = vec![expired, hidden, from_chief];
        let log = build(&plan, &[]);
        let seen: Vec<(Option<u64>, String, &str)> =
            log.entries.iter().map(|e| (e.at, e.what.word(&e.to), e.text.as_str())).collect();
        assert_eq!(
            seen,
            [
                (Some(5000), "Expired".into(), "note 7"),
                (Some(110), "Broadcast".into(), "(hidden message)"),
                (Some(100), "Message".into(), "note 7"),
                (Some(90), "Broadcast".into(), "note 9"),
            ]
        );
        assert_eq!(
            (&log.entries[3].from, &log.entries[3].to),
            (&Party::Chief, &Party::All),
            "the chief's own broadcast"
        );
        assert_eq!(log.summary.line(), "3 messages over 1 h 21 min");
        assert_eq!(
            build(&Plan::default(), &[]).summary.line(),
            "Nothing has passed between the chief and its agents yet"
        );
    }

    #[test]
    fn review_rounds_lead_with_their_count_and_the_summary_says_the_most() {
        use atomics_core::plan::{Moved, REVIEW_TASK};
        let mut plan = plan();
        plan.tasks.push(Task { id: REVIEW_TASK.into(), title: "Final review".into(), ..Task::default() });
        let report = |task: &mut Task, at: u64, summary: &str| {
            task.report = Some(Report { status: "done".into(), summary: summary.into(), at, checks: Vec::new() });
            task.set_status(TaskStatus::Review, Moved::by(Some(Decider::Agent), at));
        };
        // t1 goes back four times before it is accepted; t2 is in its second round; t3 never reached review.
        let t1 = &mut plan.tasks[0];
        for n in 0..5 {
            report(t1, 1000 + n * 100, "Done");
            if n < 4 {
                t1.set_status(
                    TaskStatus::Running,
                    Moved { by: Some(Decider::Chief), at: 1050 + n * 100, said: "Again" },
                );
            }
        }
        t1.set_status(TaskStatus::Done, Moved::by(Some(Decider::User), 1460));
        let t2 = &mut plan.tasks[1];
        report(t2, 1000, "Done");
        t2.review_feedback(None, 1010, "Rename it");
        report(t2, 1020, "Renamed");
        // The final review asks for changes once, then approves.
        let review = plan.tasks.last_mut().unwrap();
        review.set_status(TaskStatus::Running, Moved::by(None, 1500));
        report(review, 1600, "Verdict: changes, 2 blocking");
        review.set_status(TaskStatus::Running, Moved::by(Some(Decider::Chief), 1610));
        report(review, 1700, "Verdict: approve");
        let log = build(&plan, &[]);
        let card: Vec<(&str, String, u32, Option<u64>)> =
            log.reviews.iter().map(|r| (r.name(), r.words(), r.count(), r.took())).collect();
        assert_eq!(
            card,
            [
                ("t1", "accepted after 5 rounds".into(), 5, Some(460)),
                ("t2", "round 2, in review".into(), 2, None),
                ("Final review", "approved after 2 rounds".into(), 2, Some(200)),
            ]
        );
        assert_eq!((log.summary.rounds, log.summary.most.clone()), (9, Some(("t1".into(), 5))));
        assert!(log.summary.line().contains("9 review rounds, t1 took 5"), "{}", log.summary.line());
        let words: Vec<String> =
            log.entries.iter().filter(|e| matches!(e.what, What::Round { .. })).map(|e| e.what.word(&e.to)).collect();
        assert_eq!(
            &words[..4],
            [
                "Final review round 2 · approve",
                "Final review round 1 · changes requested, 2 blocking",
                "Review round 5 · accepted",
                "Review round 4 · changes requested",
            ]
        );
        assert!(words.contains(&"Review round 2 · in review".to_string()), "t2's open round");
        assert!(words.contains(&"Review round 1 · changes requested".to_string()), "t2's first");
        let changes = log.entries.iter().find(|e| e.key == "round:t1:1").unwrap();
        assert_eq!(
            (&changes.from, &changes.to, changes.text.as_str(), changes.agent),
            (&Party::Chief, &Party::Task("t1".into()), "Again", false)
        );
        // Rounds are no messages: the pairs do not count them.
        assert_eq!(log.pairs.iter().map(Pair::total).sum::<usize>(), log.summary.reports, "only each task's report");
        // One round each says no "took".
        let mut once = super::tests::plan();
        report(&mut once.tasks[0], 10, "Done");
        assert_eq!(build(&once, &[]).summary.line(), "1 report · 1 review round within a minute");
    }

    #[test]
    fn durations_read_short() {
        let cases =
            [(5, "5 s"), (125, "2 min"), (3600, "1 h"), (4320, "1 h 12 min"), (86400, "1 d"), (97200, "1 d 3 h")];
        for (secs, words) in cases {
            assert_eq!(duration(secs), words);
        }
    }

    #[test]
    fn subagents_are_counted_by_kind_from_each_tasks_session() {
        use atomics_core::projection::ThreadInfo;
        let plan = plan();
        let thread =
            |kind: &str, running| ThreadInfo { id: kind.into(), kind: kind.into(), running, steps: 3, tool: None };
        let info = ActivityInfo {
            threads: vec![thread("Explore", false), thread("general-purpose", true), thread("Explore", true)],
            ..ActivityInfo::default()
        };
        let found = subagents(&plan, |session| (session == "s-t2").then_some(&info));
        assert_eq!(
            found,
            [Subagents {
                task: "t2".into(),
                kinds: vec![("Explore".into(), 2), ("general-purpose".into(), 1)],
                running: 2
            }]
        );
        assert_eq!(found[0].total(), 3);
    }
}
