//! The UI's [`Backend`] over a remote host. The board is `atomics.v1`'s, kept in an
//! `atomics_core::mirror::Mirror`; diffs are fetched file by file with `diff_get`; and the UI's commands
//! become mutations through [`Command::remote`], each only when the host's `welcome` grants it. I/O free:
//! frames come in through [`RemoteBackend::receive`] and go out through [`RemoteBackend::take_outgoing`].
//!
//! The browser offers what atomics.v1 can carry: a command is available when [`Command::remote`] has mutations
//! for it and `welcome` grants them all, and it follows and types into terminals ([`crate::screens`]) where
//! it grants `type`. The views draw no action for any other command; one sent anyway says so on the notice
//! line, and so does a mutation the host refuses or cannot confirm, whose optimistic effects are undone.
//!
//! What a host keeps of its own window and atomics.v1 does not send (the tab, the active session, the
//! right panel, the chief's window) is the page's, set by the views from their [`crate::desk::Desk`]
//! ([`RemoteBackend::set_view`], [`RemoteBackend::set_chief_window`]); what the host cannot send at all
//! (its config, projects, worktrees, sandboxes, usage) reads as absent, so the frame's host-only items stay
//! empty.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use atomics_core::approvals::ApprovalRequest;
use atomics_core::config::{AgentKind, ChiefDock, FontConfig, KeyConfig};
use atomics_core::diff::{FileDiff, WorktreeDiff};
use atomics_core::diff_data::Prepared;
use atomics_core::layout::LayoutConfig;
use atomics_core::lease::GridSize;
use atomics_core::mirror::Mirror;
use atomics_core::ops::{
    self, Grant, Outcome, Pending, PlanAction, PlanCommand, ReviewAction, ReviewCommand, SessionAction, SessionCommand,
};
use atomics_core::plan::{Plan, PlanStatus, Report, Task, TaskStatus};
use atomics_core::projection::{DiffFileBody, DiffFileSummary, PlanInfo, SessionInfo, TaskInfo};
use atomics_core::repo_workflow::RepoWorkflow;
use atomics_core::screen::Screen;
use atomics_core::session::AgentStatus;
use atomics_core::steer::ReviewComment;
use atomics_core::wire::{self, Envelope, ErrorCode, Message, Role, Seq, VERSION};
use atomics_ui::{
    ApprovalRow, Backend, BoardView, ChiefView, Command, CompletedTile, DiffHeading, DiffSnapshot, Notices, PanelMode,
    PlanRow, PlanState, ProjectMenu, RecentSession, Remote, SandboxLoad, SessionRow, TileView, UiConfig, ViewHints,
    WorktreeRow,
};

use crate::desk::{self, ChiefWindow};
use crate::screens::{Lease, Refused, Screens};

/// What this client asks the host for.
pub const CAPABILITIES: &[&str] =
    &["board", "plans", "approvals", "diffs", wire::TERMINALS, wire::INPUT, wire::MUTATIONS];
/// Files of one worktree whose hunks are asked for at once.
const MAX_IN_FLIGHT: usize = 4;
/// Pages of one file's diff read before the rest is left out.
const MAX_PAGES: u32 = 16;
/// The agent name a host gives a plain shell. The mirror has no agent kinds, so this is only the usual shell, for
/// counting agents and for whether a worktree has one to review; nothing that types can rely on it.
const SHELL: &str = "shell";
/// What a command says when the channel is down, until it is back.
const NOT_CONNECTED: &str = "Not connected to the host. Try again once it is back.";
/// What the notice calls a steer's target that has left the mirror, as after a reset: its id means nothing to a reader.
const CLOSED_SESSION: &str = "a closed session";

/// What the host's `welcome` said about this client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    pub role: Role,
    pub may: Vec<Grant>,
    pub capabilities: Vec<String>,
}

pub struct RemoteBackend {
    state: RefCell<State>,
    /// Operation IDs: this client's random prefix and a counter.
    prefix: String,
    ops: Cell<u64>,
}

struct State {
    mirror: Mirror,
    welcome: Option<Welcome>,
    ttl_secs: u32,
    /// A `resync` is on its way: gaps until its snapshot lands are not reported again.
    resyncing: bool,
    pending: Pending,
    /// What a mutation in flight changed on screen, by operation.
    flights: HashMap<String, Flight>,
    /// Decisions the host refused or could not confirm, which the approval banner may offer again.
    refused: Vec<(String, String)>,
    /// The steers on their way, by number, and the last number given.
    steers: HashMap<u64, Steering>,
    last_steer: u64,
    /// A steer that reached none of its targets: its text, for the steer bar to hold again.
    unsent: Option<String>,
    /// Comments that did not go through, by worktree, for their boxes to hold again.
    unsent_comments: Vec<(PathBuf, ReviewComment)>,
    /// The host started a session this page asked for.
    started: bool,
    /// Diffs as fetched, by worktree.
    diffs: BTreeMap<String, Fetched>,
    /// The worktrees whose files' hunks to fetch: those on screen.
    wanted: BTreeSet<String>,
    /// `diff_get` requests in flight, by request ID: the worktree and path, to place an error.
    requests: HashMap<String, (String, String)>,
    next_request: u64,
    outgoing: Vec<Envelope>,
    notice: Option<String>,
    now_ms: u64,
    view: ViewHints,
    /// The chief's window in this page, and whether the page draws the wide layout, where it has one.
    chief: ChiefWindow,
    wide: bool,
    screens: Screens,
}

/// What a mutation in flight changed on screen: gone with its outcome, and undone if that is not `done`.
enum Flight {
    /// An approval decided here, by session and request: the banner hides it meanwhile.
    Decision(String, String),
    /// One target of a steer ([`Steering`]), by the steer's number and the session.
    Steer(u64, String),
    /// A comment that left its box, by worktree.
    Comment(PathBuf, ReviewComment),
    /// A session being started.
    Start,
    /// A task being merged, by plan and task: its card says so meanwhile.
    Integrate(String, String),
}

/// A steer on its way, until each of its targets has answered.
struct Steering {
    text: String,
    waiting: usize,
    reached_any: bool,
    /// The targets it missed, each title once, and why the last of them did.
    missed: Vec<String>,
    why: Option<String>,
}

/// What a mutation that went through did not do yet, where the screen does not show it. The rest shows.
fn delivery_note(result: &serde_json::Value) -> Option<String> {
    let yes = |field: &str| result.get(field).and_then(serde_json::Value::as_bool).unwrap_or(false);
    if yes("held") {
        Some("Held until the session is ready.".into())
    } else if yes("queued_until_next_hook") {
        Some("Queued for an agent outside ATOMICS: it reads it at its next tool call or turn end.".into())
    } else {
        None
    }
}

/// One worktree's diff at one revision.
struct Fetched {
    revision: u64,
    files: BTreeMap<String, Body>,
    /// Built from the summary and the bodies so far; cleared when either changes.
    prepared: Option<Arc<Prepared>>,
}

enum Body {
    Asked,
    Got { file: FileDiff, pages: u32, more: bool },
    Failed,
}

impl Fetched {
    fn new(revision: u64) -> Self {
        Self { revision, files: BTreeMap::new(), prepared: None }
    }
}

/// The path a file's diff is asked for and answered by.
fn summary_path(file: &DiffFileSummary) -> String {
    file.new_path.clone().or_else(|| file.old_path.clone()).unwrap_or_default()
}

fn file_path(file: &FileDiff) -> String {
    file.new_path.clone().or_else(|| file.old_path.clone()).unwrap_or_default()
}

fn default_hints() -> ViewHints {
    ViewHints {
        active: None,
        diff_target: None,
        plan_view: true,
        ungrouped: false,
        project: None,
        selected_plan: 0,
        diff_open: false,
        panel: PanelMode::Plan,
    }
}

impl RemoteBackend {
    /// A backend with nothing in it yet. `prefix` starts every operation ID this client makes; give each
    /// page a random one, so two pages never reuse an ID.
    pub fn new(prefix: &str) -> Self {
        let state = State {
            mirror: Mirror::default(),
            welcome: None,
            ttl_secs: ops::OP_TTL_SECS,
            resyncing: false,
            pending: Pending::default(),
            flights: HashMap::new(),
            refused: Vec::new(),
            steers: HashMap::new(),
            last_steer: 0,
            unsent: None,
            unsent_comments: Vec::new(),
            started: false,
            diffs: BTreeMap::new(),
            wanted: BTreeSet::new(),
            requests: HashMap::new(),
            next_request: 0,
            outgoing: Vec::new(),
            notice: None,
            now_ms: 0,
            view: default_hints(),
            chief: ChiefWindow { open: false, dock: ChiefDock::Floating, width: None },
            wide: false,
            screens: Screens::default(),
        };
        Self { state: RefCell::new(state), prefix: prefix.to_string(), ops: Cell::new(0) }
    }

    fn next_op(&self) -> String {
        let n = self.ops.get() + 1;
        self.ops.set(n);
        format!("{}-{n}", self.prefix)
    }

    /// The clock, in milliseconds on any clock that only goes forward: how old an operation is.
    pub fn set_now(&self, now_ms: u64) {
        self.state.borrow_mut().now_ms = now_ms;
    }

    /// The first frame on a new channel: `hello`, resuming from what the board holds.
    pub fn hello(&self, client_id: &str) -> Envelope {
        let (prior_epoch, prior_cursor) = self.state.borrow().mirror.resume_point();
        Envelope::new(Message::Hello {
            client_id: client_id.to_string(),
            versions: vec![VERSION],
            capabilities: CAPABILITIES.iter().map(|c| c.to_string()).collect(),
            prior_epoch,
            prior_cursor: prior_cursor.map(Seq),
        })
    }

    /// The channel ended. The board stays as it was, to be resumed; frames not sent yet are dropped, and
    /// operations in flight are sent again after the next `welcome` if they are young enough.
    pub fn disconnected(&self) {
        let mut s = self.state.borrow_mut();
        s.welcome = None;
        s.resyncing = false;
        s.outgoing.clear();
        s.requests.clear();
        s.screens.disconnected();
        for fetched in s.diffs.values_mut() {
            fetched.files.retain(|_, body| matches!(body, Body::Got { more: false, .. } | Body::Failed));
        }
    }

    /// What the host's `welcome` said, while connected.
    pub fn welcome(&self) -> Option<Welcome> {
        self.state.borrow().welcome.clone()
    }

    /// Whether the host lets this client do `grant`.
    pub fn may(&self, grant: Grant) -> bool {
        self.state.borrow().welcome.as_ref().is_some_and(|w| w.may.contains(&grant))
    }

    /// A read of the board as the mirror holds it.
    pub fn with_mirror<R>(&self, read: impl FnOnce(&Mirror) -> R) -> R {
        read(&self.state.borrow().mirror)
    }

    /// The frames to send, in order.
    pub fn take_outgoing(&self) -> Vec<Envelope> {
        std::mem::take(&mut self.state.borrow_mut().outgoing)
    }

    /// The approvals decided here that the host refused or could not confirm, by session and request.
    pub fn take_refused(&self) -> Vec<(String, String)> {
        std::mem::take(&mut self.state.borrow_mut().refused)
    }

    /// The text of a steer that reached none of its targets, for the steer bar to hold again.
    pub fn take_unsent(&self) -> Option<String> {
        self.state.borrow_mut().unsent.take()
    }

    /// The comments that did not go through, by worktree, for their boxes to hold again.
    pub fn take_unsent_comments(&self) -> Vec<(PathBuf, ReviewComment)> {
        std::mem::take(&mut self.state.borrow_mut().unsent_comments)
    }

    /// Whether the host started a session this page asked for since the last time this was asked.
    pub fn take_started(&self) -> bool {
        std::mem::take(&mut self.state.borrow_mut().started)
    }

    pub fn notice(&self) -> Option<String> {
        self.state.borrow().notice.clone()
    }

    pub fn dismiss_notice(&self) {
        self.state.borrow_mut().notice = None;
    }

    /// Say something on the notice line.
    pub fn say(&self, notice: impl Into<String>) {
        self.state.borrow_mut().notice = Some(notice.into());
    }

    /// What is on screen, which the UI reads back through [`Backend::view_hints`].
    pub fn set_view(&self, hints: ViewHints) {
        self.state.borrow_mut().view = hints;
    }

    /// The chief's window in this page, which the UI reads back through [`Backend::chief`], and whether the
    /// page draws the wide layout, the only one with a window to show, dock or resize.
    pub fn set_chief_window(&self, chief: ChiefWindow, wide: bool) {
        let mut s = self.state.borrow_mut();
        (s.chief, s.wide) = (chief, wide);
    }

    /// Fetch the hunks of `worktree`'s files, the diff on screen, and of no other worktree.
    pub fn set_wanted(&self, worktree: Option<&Path>) {
        let mut s = self.state.borrow_mut();
        let wanted: BTreeSet<String> = worktree.map(|w| w.to_string_lossy().into_owned()).into_iter().collect();
        if s.wanted != wanted {
            s.wanted = wanted;
            s.fetch();
        }
    }

    /// Fetch the hunks of `worktree`'s changed files: its diff is on screen.
    pub fn want_diff(&self, worktree: &Path) {
        let mut s = self.state.borrow_mut();
        s.wanted.insert(worktree.to_string_lossy().into_owned());
        s.fetch();
    }

    /// Stop fetching `worktree`'s files.
    pub fn unwant_diff(&self, worktree: &Path) {
        self.state.borrow_mut().wanted.remove(worktree.to_string_lossy().as_ref());
    }

    /// Fetch `worktree`'s files again.
    pub fn refetch(&self, worktree: &Path) {
        let mut s = self.state.borrow_mut();
        let key = worktree.to_string_lossy().into_owned();
        if let Some(fetched) = s.diffs.get_mut(&key) {
            let revision = fetched.revision;
            *fetched = Fetched::new(revision);
        }
        s.requests.retain(|_, (w, _)| *w != key);
        s.wanted.insert(key);
        s.fetch();
    }

    /// One frame from the host.
    pub fn receive(&self, envelope: Envelope) {
        let mut s = self.state.borrow_mut();
        let s = &mut *s;
        if s.screens.receive(&envelope, &mut s.outgoing) {
            return;
        }
        s.receive(envelope);
        s.fetch();
    }

    /// The plan a ticket key names, by its place in the plan list: an open plan that follows the ticket,
    /// else an open plan with a task for it, else a closed plan that does either. Keys compare without
    /// regard to ASCII case. `None` when no plan names it.
    pub fn plan_for_ticket(&self, key: &str) -> Option<usize> {
        let s = self.state.borrow();
        let names = |ticket: &Option<String>| ticket.as_deref().is_some_and(|t| t.eq_ignore_ascii_case(key));
        let own = |p: &PlanInfo| names(&p.ticket);
        let task = |p: &PlanInfo| p.tasks.iter().any(|t| names(&t.ticket));
        let open = |p: &PlanInfo| p.status == PlanStatus::Open;
        let rules: [&dyn Fn(&PlanInfo) -> bool; 3] =
            [&|p| open(p) && own(p), &|p| open(p) && task(p), &|p| own(p) || task(p)];
        rules.iter().find_map(|rule| s.mirror.plans.values().position(rule))
    }

    /// The folders this page can name for a new session, sorted: the open plans' projects and where the host's
    /// sessions run. The host's own list of projects and worktrees stays on the host.
    pub fn folders(&self) -> Vec<PathBuf> {
        let s = self.state.borrow();
        let projects =
            s.mirror.plans.values().filter(|p| p.status == PlanStatus::Open).filter_map(|p| p.project.as_ref());
        let known: BTreeSet<PathBuf> =
            projects.map(PathBuf::from).chain(s.mirror.sessions.values().map(root)).collect();
        known.into_iter().collect()
    }

    /// Connected and greeted: frames sent now are read.
    pub fn live(&self) -> bool {
        self.state.borrow().welcome.is_some()
    }

    /// The UI shows `session`'s terminal this frame ([`Screens::show`]).
    pub fn show_screen(&self, session: &str, focused: bool) {
        let mut s = self.state.borrow_mut();
        let s = &mut *s;
        let live = s.welcome.is_some();
        s.screens.show(session, focused, live, &mut s.outgoing);
    }

    /// After each frame: stop following the screens it did not show.
    pub fn end_frame(&self) {
        let mut s = self.state.borrow_mut();
        let s = &mut *s;
        s.screens.end_frame(&mut s.outgoing);
    }

    /// A read of `session`'s screen as received, with whether it is from before a disconnect.
    pub fn with_screen<R>(&self, session: &str, read: impl FnOnce(Option<(&Screen, bool)>) -> R) -> R {
        read(self.state.borrow().screens.view(session))
    }

    /// Fill `view` with `rows` lines of `session` from line ID `top` on ([`Screens::fill`]).
    pub fn fill_screen(&self, session: &str, top: u64, rows: u16, view: &mut Screen) -> bool {
        let mut s = self.state.borrow_mut();
        let s = &mut *s;
        s.screens.fill(session, top, rows, view, &mut s.outgoing).is_some()
    }

    pub fn lease(&self, session: &str) -> Lease {
        self.state.borrow().screens.lease(session)
    }

    /// Ask for `session`'s lease; `takeover` only for the user's explicit act. Not for a view-only device.
    pub fn acquire(&self, session: &str, takeover: bool) {
        if !self.may(Grant::Type) {
            return;
        }
        let mut s = self.state.borrow_mut();
        let s = &mut *s;
        s.screens.acquire(session, takeover, &mut s.outgoing);
    }

    /// The input left `session`'s terminal: let go of its lease ([`Screens::release`]).
    pub fn release(&self, session: &str) {
        let mut s = self.state.borrow_mut();
        let s = &mut *s;
        s.screens.release(session, &mut s.outgoing);
    }

    /// Type `data` into `session`, already encoded for its modes, while this client holds the lease or is
    /// asking for it ([`Screens::type_text`]). Nothing is sent without the `type` grant or a channel, and
    /// nothing refused is kept for later: typing never asks for the lease.
    pub fn type_text(&self, session: &str, data: &str) -> Result<(), Refused> {
        if !self.may(Grant::Type) {
            return Err(Refused::NoLease);
        }
        let mut s = self.state.borrow_mut();
        let s = &mut *s;
        s.screens.type_text(session, data, &mut s.outgoing)
    }

    /// A read of the terminals' own state, for tests.
    #[cfg(test)]
    pub(crate) fn with_screens<R>(&self, read: impl FnOnce(&Screens) -> R) -> R {
        read(&self.state.borrow().screens)
    }

    pub fn renew(&self, session: &str) {
        let mut s = self.state.borrow_mut();
        let s = &mut *s;
        s.screens.renew(session, &mut s.outgoing);
    }

    /// Fit `session` to this screen with a size, or give it back the computer's size with `None`.
    pub fn resize(&self, session: &str, size: Option<GridSize>) {
        let mut s = self.state.borrow_mut();
        let s = &mut *s;
        s.screens.resize(session, size, &mut s.outgoing);
    }
}

impl State {
    /// An operation ended: what it changed on screen goes, undone unless it is `done`.
    fn land(&mut self, op: &str, done: bool) {
        match self.flights.remove(op) {
            Some(Flight::Decision(session, approval)) if !done => self.refused.push((session, approval)),
            Some(Flight::Steer(steer, session)) => self.steer_answered(steer, &session, done),
            Some(Flight::Comment(root, comment)) if !done => self.unsent_comments.push((root, comment)),
            Some(Flight::Start) if done => self.started = true,
            _ => {}
        }
    }

    /// One target of a steer answered, with the reason on the notice line if it was missed. Once all have: reached
    /// by none, the text goes back to the bar. Reached by some, the bar stays empty, since Enter would reach those
    /// again, and the note names the others.
    fn steer_answered(&mut self, steer: u64, session: &str, done: bool) {
        let Some(steering) = self.steers.get_mut(&steer) else { return };
        steering.waiting -= 1;
        if done {
            steering.reached_any = true;
        } else {
            steering.why = self.notice.clone();
            let title = self.mirror.sessions.get(session).map_or(CLOSED_SESSION, |s| s.title.as_str());
            if !steering.missed.iter().any(|missed| missed == title) {
                steering.missed.push(title.to_string());
            }
        }
        if steering.waiting > 0 {
            return;
        }
        let Some(steering) = self.steers.remove(&steer) else { return };
        if !steering.reached_any {
            self.unsent = Some(steering.text);
        } else if !steering.missed.is_empty() {
            let why = steering.why.unwrap_or_default();
            self.notice = Some(format!("Missed by {}. {why}", steering.missed.join(", ")));
        }
    }

    fn resync(&mut self) {
        if !self.resyncing {
            self.resyncing = true;
            self.outgoing.push(Envelope::new(Message::Resync { reason: Some("gap".into()), screens: Vec::new() }));
        }
    }

    fn receive(&mut self, envelope: Envelope) {
        match &envelope.message {
            Message::Welcome { capabilities, limits, role, may, .. } => {
                self.welcome = Some(Welcome { role: *role, may: may.clone(), capabilities: capabilities.clone() });
                if limits.op_ttl_secs > 0 {
                    self.ttl_secs = limits.op_ttl_secs;
                }
                self.resyncing = false;
                if self.mirror.apply(&envelope).is_err() {
                    // A replay that does not follow the board held: start again from a snapshot.
                    self.resync();
                }
                // The connection is back, so what said it was not goes.
                if self.notice.as_deref() == Some(NOT_CONNECTED) {
                    self.notice = None;
                }
                let (again, expired) = self.pending.resend(self.now_ms, self.ttl_secs);
                self.outgoing.extend(again.into_iter().map(Envelope::new));
                if !expired.is_empty() {
                    self.notice = Some(
                        "Some earlier actions may not have reached the host. Check the board before trying again."
                            .into(),
                    );
                }
                for command in &expired {
                    self.land(command.op(), false);
                }
            }
            Message::DiffFile { body, truncated, next_offset } => self.diff_page(body, *truncated, *next_offset),
            Message::CommandResult(result) => {
                let Some(outcome) = self.pending.apply(result) else { return };
                match &outcome {
                    Outcome::Refused { code: ErrorCode::AlreadyResolved, message } => {
                        self.notice = Some(format!("That request was already answered: {message}"));
                    }
                    Outcome::Refused { code: ErrorCode::Unauthorised, .. } => {
                        self.notice = Some("The host does not let this device do that.".into());
                    }
                    Outcome::Refused { message, .. } => self.notice = Some(format!("The host refused: {message}")),
                    Outcome::Uncertain { message } => {
                        self.notice = Some(format!(
                            "The host could not confirm it ({message}). Check the board before trying again."
                        ));
                    }
                    Outcome::Done { .. } | Outcome::Accepted | Outcome::Unknown => {}
                }
                self.land(&result.op, matches!(outcome, Outcome::Done { .. }));
                // It never takes the place of what went wrong with another.
                if let Outcome::Done { result } = &outcome {
                    self.notice = self.notice.take().or_else(|| delivery_note(result));
                }
            }
            Message::Ping { nonce } => {
                let pong = Envelope::new(Message::Pong { nonce: nonce.clone() }).reply_to(envelope.request_id.clone());
                self.outgoing.push(pong);
            }
            Message::Error { code, message } => {
                let request = envelope.request_id.as_deref().and_then(|id| self.requests.remove(id));
                match request {
                    Some((worktree, path)) => {
                        let Some(fetched) = self.diffs.get_mut(&worktree) else { return };
                        if *code == ErrorCode::StaleRevision {
                            // The diff moved on; its new summary brings a revision to ask at.
                            fetched.files.remove(&path);
                        } else {
                            fetched.files.insert(path, Body::Failed);
                        }
                        fetched.prepared = None;
                    }
                    None => self.notice = Some(format!("The host refused a request: {message}")),
                }
            }
            _ => match self.mirror.apply(&envelope) {
                Ok(atomics_core::mirror::Applied::Changed)
                    if matches!(envelope.message, Message::StateSnapshot { .. }) =>
                {
                    self.resyncing = false;
                }
                Ok(_) => {}
                Err(_) => self.resync(),
            },
        }
    }

    /// One page of a file's hunks.
    fn diff_page(&mut self, body: &DiffFileBody, truncated: bool, next_offset: Option<u32>) {
        let Some(fetched) = self.diffs.get_mut(&body.worktree) else { return };
        if body.revision != fetched.revision {
            return;
        }
        let path = file_path(&body.file);
        let Some(entry) = fetched.files.get_mut(&path) else { return };
        let pages = match entry {
            Body::Asked => {
                *entry = Body::Got { file: body.file.clone(), pages: 1, more: false };
                1
            }
            Body::Got { file, pages, more } if *more => {
                *more = false;
                *pages += 1;
                // A hunk cut at a page boundary goes on under the same header.
                let mut hunks = body.file.hunks.iter().cloned();
                if let (Some(last), Some(first)) = (file.hunks.last_mut(), body.file.hunks.first())
                    && last.header == first.header
                {
                    last.lines.extend(first.lines.iter().cloned());
                    hunks.next();
                }
                file.hunks.extend(hunks);
                *pages
            }
            _ => return,
        };
        fetched.prepared = None;
        if let (true, Some(offset)) = (truncated, next_offset)
            && pages < MAX_PAGES
        {
            if let Some(Body::Got { more, .. }) = fetched.files.get_mut(&path) {
                *more = true;
            }
            let worktree = body.worktree.clone();
            let revision = Some(Seq(body.revision));
            let message =
                Message::DiffGet { worktree: worktree.clone(), path: path.clone(), revision, offset: Some(offset) };
            self.request(message, worktree, path);
        }
    }

    fn request(&mut self, message: Message, worktree: String, path: String) {
        self.next_request += 1;
        let id = format!("diff-{}", self.next_request);
        self.requests.insert(id.clone(), (worktree, path));
        self.outgoing.push(Envelope::new(message).reply_to(Some(id)));
    }

    /// Ask for the hunks of the wanted worktrees' files, a few at a time per worktree.
    fn fetch(&mut self) {
        if self.welcome.is_none() {
            return;
        }
        let mut asks = Vec::new();
        for worktree in &self.wanted {
            let Some(summary) = self.mirror.diffs.get(worktree) else { continue };
            let fetched = self.diffs.entry(worktree.clone()).or_insert_with(|| Fetched::new(summary.revision));
            if fetched.revision != summary.revision {
                *fetched = Fetched::new(summary.revision);
            }
            let mut in_flight =
                fetched.files.values().filter(|b| matches!(b, Body::Asked | Body::Got { more: true, .. })).count();
            for file in &summary.files {
                if in_flight >= MAX_IN_FLIGHT {
                    break;
                }
                let path = summary_path(file);
                if file.binary || path.is_empty() || fetched.files.contains_key(&path) {
                    continue;
                }
                fetched.files.insert(path.clone(), Body::Asked);
                in_flight += 1;
                asks.push((worktree.clone(), path, summary.revision));
            }
        }
        for (worktree, path, revision) in asks {
            let message = Message::DiffGet {
                worktree: worktree.clone(),
                path: path.clone(),
                revision: Some(Seq(revision)),
                offset: None,
            };
            self.request(message, worktree, path);
        }
    }

    fn snapshot(&mut self, worktree: &str) -> Option<DiffSnapshot> {
        let summary = self.mirror.diffs.get(worktree)?;
        let fetched = self.diffs.entry(worktree.to_string()).or_insert_with(|| Fetched::new(summary.revision));
        if fetched.revision != summary.revision {
            *fetched = Fetched::new(summary.revision);
        }
        let prepared = match &fetched.prepared {
            Some(prepared) => prepared.clone(),
            None => {
                let files: Vec<FileDiff> = summary
                    .files
                    .iter()
                    .map(|f| match fetched.files.get(&summary_path(f)) {
                        Some(Body::Got { file, .. }) => file.clone(),
                        _ => FileDiff {
                            old_path: f.old_path.clone(),
                            new_path: f.new_path.clone(),
                            status: f.status,
                            binary: f.binary,
                            hunks: Vec::new(),
                            omitted: f.omitted,
                        },
                    })
                    .collect();
                let longest_line = files
                    .iter()
                    .flat_map(|f| f.hunks.iter().flat_map(|h| h.lines.iter()))
                    .map(|l| l.text.chars().count() + l.text.matches('\t').count() * 3)
                    .chain(files.iter().map(|f| f.path().len() + 20))
                    .max()
                    .unwrap_or(0);
                let prepared = Arc::new(Prepared {
                    highlights: files.iter().map(|_| Arc::new(Vec::new())).collect(),
                    longest_line,
                    additions: summary.additions(),
                    deletions: summary.deletions(),
                    all_files: Arc::default(),
                    diff: WorktreeDiff {
                        base_ref: summary.base_ref.clone(),
                        base: summary.base.clone(),
                        files,
                        partial: summary.partial,
                    },
                });
                fetched.prepared = Some(prepared.clone());
                prepared
            }
        };
        let loading = self.wanted.contains(worktree)
            && summary.files.iter().any(|f| {
                !f.binary
                    && !matches!(
                        fetched.files.get(&summary_path(f)),
                        Some(Body::Got { more: false, .. } | Body::Failed)
                    )
            });
        let failed = fetched.files.values().filter(|b| matches!(b, Body::Failed)).count();
        Some(DiffSnapshot {
            revision: summary.revision,
            prepared: Some(prepared),
            error: (failed > 0).then(|| format!("{failed} file(s) could not be read from the host")),
            loading,
            comments: summary.comments.clone(),
            viewing: None,
            file: None,
            file_error: None,
        })
    }
}

fn plan_state(plan: &PlanInfo, mirror: &Mirror) -> PlanState {
    match plan.status {
        PlanStatus::Completed => return PlanState::Completed,
        PlanStatus::Archived => return PlanState::Archived,
        PlanStatus::Open => {}
    }
    let linked = || {
        mirror.sessions.values().filter(move |s| !s.chief && s.task.as_ref().is_some_and(|link| link.plan == plan.id))
    };
    let stuck = plan.tasks.iter().any(|task| {
        task.status == TaskStatus::Blocked
            || task.status != TaskStatus::Done && task.report.as_ref().is_some_and(|r| r.status == "question")
    });
    if stuck || linked().any(|s| s.status == AgentStatus::NeedsInput || s.pending_approvals > 0) {
        return PlanState::NeedsYou;
    }
    match linked().filter(|s| s.status == AgentStatus::Working).count() {
        0 => PlanState::Idle,
        working => PlanState::Running(working),
    }
}

fn plan_row(plan: &PlanInfo, mirror: &Mirror) -> PlanRow {
    let count = |status| plan.tasks.iter().filter(|t| t.status == status).count();
    PlanRow {
        id: plan.id.clone(),
        goal: plan.goal.clone(),
        tab_name: plan.goal.clone(),
        status: plan.status,
        state: plan_state(plan, mirror),
        changed: false,
        done: count(TaskStatus::Done),
        total: plan.tasks.len(),
        attention: count(TaskStatus::Review) + count(TaskStatus::Blocked),
    }
}

fn task(info: &TaskInfo) -> Task {
    Task {
        id: info.id.clone(),
        title: info.title.clone(),
        depends_on: info.depends_on.clone(),
        agent: info.agent.clone(),
        branch: info.branch.clone(),
        status: info.status,
        integrated: info.integrated.clone(),
        // The host checks whether its branch still holds the task; the mirror says what it last found.
        integration_verified: info.integrated.is_some(),
        session: info.session.clone(),
        additions: info.additions,
        deletions: info.deletions,
        report: info.report.as_ref().map(|r| Report { status: r.status.clone(), summary: r.summary.clone(), at: r.at }),
        ..Task::default()
    }
}

/// The plan as its projection carries it: briefs, acceptance and the host's own bookkeeping stay there.
fn plan(info: &PlanInfo) -> Plan {
    Plan {
        id: info.id.clone(),
        status: info.status,
        goal: info.goal.clone(),
        project: info.project.as_ref().map(PathBuf::from),
        tasks: info.tasks.iter().map(task).collect(),
        approved: info.approved,
        integration: info.integration.clone(),
        ..Plan::default()
    }
}

fn root(session: &SessionInfo) -> PathBuf {
    PathBuf::from(session.worktree.as_deref().unwrap_or(&session.cwd))
}

fn session_row(session: &SessionInfo, mirror: &Mirror) -> SessionRow {
    let root = root(session);
    SessionRow {
        id: session.id.clone(),
        title: session.title.clone(),
        agent: session.agent.clone(),
        kind: if session.agent == SHELL { AgentKind::Shell } else { AgentKind::Generic },
        status: session.status,
        chief: session.chief,
        worktree: session.worktree.as_ref().map(PathBuf::from),
        branch: session.branch.clone(),
        project: None,
        ungrouped: !session.chief && session.task.is_none(),
        scratch: false,
        sandbox: None,
        outside: false,
        can_receive: session.status != AgentStatus::Exited,
        pending_approvals: session.pending_approvals as usize,
        held_messages: 0,
        changes: mirror.diffs.get(root.to_string_lossy().as_ref()).map(|d| (d.additions(), d.deletions())),
        root,
    }
}

/// The agents this page can name for a new session, sorted: those the host runs and those its open plans use.
/// The host's configured list stays on the host, which refuses an agent it no longer has.
fn known_agents(mirror: &Mirror) -> Vec<String> {
    let tasks = mirror.plans.values().filter(|p| p.status == PlanStatus::Open).flat_map(|p| &p.tasks);
    let names = mirror.sessions.values().map(|s| &s.agent).chain(tasks.map(|t| &t.agent));
    let known: BTreeSet<&String> = names.filter(|name| !name.is_empty()).collect();
    known.into_iter().cloned().collect()
}

impl Backend for RemoteBackend {
    fn send(&self, command: Command) {
        if let Command::RefreshDiff(root) = &command {
            return self.refetch(root);
        }
        if self.welcome().is_none() {
            self.refuse(&command);
            return self.say(NOT_CONNECTED);
        }
        let mut op = || self.next_op();
        let mutations = match self.remote_of(&command, &mut op) {
            Remote::Send(mutations) => mutations,
            Remote::Client => return,
            // The views do not draw these; this is for a command sent some other way.
            Remote::Unsupported => {
                self.refuse(&command);
                return self.say("That is not available in the browser; use ATOMICS on the computer.");
            }
        };
        if mutations.is_empty() {
            self.refuse(&command);
            return self.say("That is no longer on the host. Check the board.");
        }
        let steer = self.steering(&command, mutations.len());
        for mutation in mutations {
            let refusal = if !self.may(mutation.grant()) {
                Some("This device may only view this host. Its owner decides what it may change.".to_string())
            } else if let Err(error) = mutation.validate() {
                // What the host would refuse for its size says so here, and the operation waits for nothing.
                Some(format!("That was not sent: {}.", error.message))
            } else if self.state.borrow().pending.len() >= wire::MAX_PENDING_OPS {
                // As many as the host takes at once (more are `too_many`): what waits stays bounded, offline too.
                Some("Too many changes are still waiting for the host. Try again once they are answered.".to_string())
            } else {
                None
            };
            let mut s = self.state.borrow_mut();
            if let Some(flight) = self.flight(&s.mirror, &mutation, &command, steer) {
                s.flights.insert(mutation.op().to_string(), flight);
            }
            if let Some(refusal) = refusal {
                s.notice = Some(refusal);
                s.land(mutation.op(), false);
                continue;
            }
            let now = s.now_ms;
            let message = s.pending.send(mutation, now);
            s.outgoing.push(Envelope::new(message));
        }
    }

    /// The view commands the frame carries out, a diff's refresh, and what can go to the host now
    /// ([`RemoteBackend::sends`]). Offline nothing can be sent.
    fn available(&self, command: &Command) -> bool {
        let wide = self.state.borrow().wide;
        let start = || Command::Start { agent: String::new(), folder: None };
        match command {
            Command::RefreshDiff(_) => return true,
            // The changed-files column, beside the wide layout's diff: a click scrolls to the file there.
            Command::OpenFile { .. } => return wide,
            // Only the wide layout has a steer bar; a phone's terminal is right there.
            Command::Steer { .. } if !wide => return false,
            // The wide layout's picker, of what the mirror names.
            Command::NewSession => return wide && self.sends(&start()),
            // An open plan's tab asks, and archives. A finished one's has no close; its board's Archive does that.
            Command::ClosePlanTab(plan) => {
                let open = self.plan_row(*plan).is_some_and(|row| row.status == PlanStatus::Open);
                return open && self.sends(&Command::ClosePlan { plan: *plan, status: PlanStatus::Archived });
            }
            _ => {}
        }
        if command.is_view() || desk::window_command(command) {
            let s = self.state.borrow();
            let chief = || s.mirror.sessions.values().any(|x| x.chief);
            // The plan's board and button need a plan to show: a host may have none, or none open.
            let plan = matches!(command, Command::TogglePlan | Command::SetPanel(PanelMode::Plan));
            return desk::carries_out(command, s.wide)
                && (!desk::window_command(command) || chief())
                && (!plan || s.mirror.plans.values().nth(s.view.selected_plan).is_some());
        }
        self.sends(command)
    }

    /// The mirror has no agent kinds: a shell can run under any name.
    fn tells_agents_from_shells(&self) -> bool {
        false
    }

    /// A page has no keyboard chords of its own: the keyboard goes to the terminal or to the browser.
    fn has_shortcuts(&self) -> bool {
        false
    }

    fn plan_list(&self) -> Vec<PlanRow> {
        let s = self.state.borrow();
        s.mirror.plans.values().map(|p| plan_row(p, &s.mirror)).collect()
    }

    fn plan_row(&self, index: usize) -> Option<PlanRow> {
        let s = self.state.borrow();
        s.mirror.plans.values().nth(index).map(|p| plan_row(p, &s.mirror))
    }

    fn board(&self, index: usize) -> Option<BoardView> {
        let s = self.state.borrow();
        let info = s.mirror.plans.values().nth(index)?;
        let plan = plan(info);
        let integrating = s.flights.values().filter_map(|flight| match flight {
            Flight::Integrate(plan, task) if *plan == info.id => Some(task.clone()),
            _ => None,
        });
        Some(BoardView {
            integrating: integrating.collect(),
            state: plan_state(info, &s.mirror),
            ready: plan.ready().iter().map(|t| t.id.clone()).collect(),
            sessions: s.mirror.sessions.values().map(|x| (x.id.clone(), (x.title.clone(), x.status))).collect(),
            integration_notes: HashMap::new(),
            checking: false,
            ticket_config: None,
            board_counts: None,
            worktrees: HashSet::new(),
            codex: HashSet::new(),
            now: s.now_ms / 1000,
            plan,
        })
    }

    fn plan_digest(&self, index: usize) -> Option<String> {
        let s = self.state.borrow();
        s.mirror.plans.values().nth(index).map(|p| p.digest.clone()).filter(|d| !d.is_empty())
    }

    fn session_list(&self) -> Vec<SessionRow> {
        let s = self.state.borrow();
        s.mirror.sessions.values().map(|x| session_row(x, &s.mirror)).collect()
    }

    fn session_row(&self, index: usize) -> Option<SessionRow> {
        let s = self.state.borrow();
        self.session_row_in(&s.mirror, index)
    }

    fn session_approval(&self, index: usize) -> Option<ApprovalRow> {
        let s = self.state.borrow();
        let session = s.mirror.sessions.values().nth(index)?;
        let approval = s.mirror.approvals.values().find(|a| a.session == session.id)?;
        Some(ApprovalRow {
            session: session.id.clone(),
            id: approval.id.clone(),
            request: ApprovalRequest {
                tool: approval.tool.clone(),
                command: approval.command.clone(),
                cwd: PathBuf::from(&approval.cwd),
                reason: approval.reason.clone(),
                input: serde_json::Value::Null,
            },
        })
    }

    /// The mirror says only whether the session exited and where it runs; reports, the sandbox, the slot
    /// queue and MCP servers are the host's.
    fn tile(&self, index: usize) -> Option<TileView> {
        let s = self.state.borrow();
        let session = s.mirror.sessions.values().nth(index)?;
        Some(TileView {
            exited: session.status == AgentStatus::Exited,
            folder: session.cwd.clone(),
            ..TileView::default()
        })
    }

    /// None: whether a finished task's worktree is still there is the host's to know.
    fn completed_tasks(&self) -> Vec<CompletedTile> {
        Vec::new()
    }

    fn diff_roots(&self) -> Vec<PathBuf> {
        self.state.borrow().mirror.diffs.keys().map(PathBuf::from).collect()
    }

    fn diff(&self, root: &Path) -> Option<DiffSnapshot> {
        self.state.borrow_mut().snapshot(root.to_string_lossy().as_ref())
    }

    /// The chief's session as the mirror has it, in this page's window ([`RemoteBackend::set_chief_window`]).
    /// The host's heartbeat is not sent.
    fn chief(&self) -> ChiefView {
        let s = self.state.borrow();
        let session = s.mirror.sessions.values().position(|x| x.chief);
        let ChiefWindow { open, dock, width } = s.chief;
        ChiefView { session, open: open && session.is_some(), dock, width, heartbeat: None }
    }

    fn diff_heading(&self, root: &Path) -> DiffHeading {
        let s = self.state.borrow();
        let here = |x: &&SessionInfo| self::root(x) == root;
        let session = s
            .view
            .active
            .as_ref()
            .and_then(|id| s.mirror.sessions.get(id))
            .filter(here)
            .or_else(|| s.mirror.sessions.values().find(here));
        let title = session
            .map(|x| x.title.clone())
            .unwrap_or_else(|| root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        let branch = session.and_then(|x| x.branch.clone()).filter(|b| *b != title);
        let can_send =
            s.mirror.sessions.values().filter(here).any(|x| x.agent != SHELL && x.status != AgentStatus::Exited);
        DiffHeading { title, branch, can_send }
    }

    fn ui_config(&self) -> UiConfig {
        UiConfig {
            keys: KeyConfig::default(),
            font: FontConfig::default(),
            layout: LayoutConfig::default(),
            agents: known_agents(&self.state.borrow().mirror),
        }
    }

    fn view_hints(&self) -> ViewHints {
        self.state.borrow().view.clone()
    }

    /// The tab's sessions, as the desktop's rule picks them: a plan's tab its tasks' sessions, the Ungrouped
    /// tab every session outside a plan, else all; never the chief.
    fn visible_sessions(&self) -> Vec<usize> {
        let s = self.state.borrow();
        let plan = s.view.plan_view.then(|| s.mirror.plans.values().nth(s.view.selected_plan)).flatten();
        let in_plan = |x: &SessionInfo, plan: &PlanInfo| {
            x.task.as_ref().is_some_and(|link| link.plan == plan.id)
                || plan.tasks.iter().any(|t| t.session.as_deref() == Some(x.id.as_str()))
        };
        let shown = |x: &SessionInfo| match plan {
            Some(plan) => in_plan(x, plan),
            None if s.view.ungrouped => x.task.is_none(),
            None => true,
        };
        s.mirror.sessions.values().enumerate().filter(|(_, x)| !x.chief && shown(x)).map(|(i, _)| i).collect()
    }

    fn checkouts(&self) -> Vec<String> {
        Vec::new()
    }

    fn recent_sessions(&self) -> Vec<RecentSession> {
        Vec::new()
    }

    fn repos(&self) -> Vec<PathBuf> {
        Vec::new()
    }

    fn project_of(&self, _dir: &Path) -> Option<PathBuf> {
        None
    }

    fn repo_workflow(&self, _project: &Path) -> Option<RepoWorkflow> {
        None
    }

    fn worktrees(&self, _project: &Path) -> Vec<WorktreeRow> {
        Vec::new()
    }

    fn display(&self, path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    fn plan_date(&self, _index: usize) -> String {
        String::new()
    }

    /// None: the top bar's narrow slot would cut them. The page shows them whole ([`RemoteBackend::notice`]).
    fn notices(&self) -> Notices {
        Notices::default()
    }

    fn sandbox_load(&self) -> SandboxLoad {
        SandboxLoad::default()
    }

    fn project_menu(&self) -> ProjectMenu {
        ProjectMenu::default()
    }

    /// The host's folders are its own.
    fn launch_dir(&self) -> Option<PathBuf> {
        None
    }
}

impl RemoteBackend {
    fn session_row_in(&self, mirror: &Mirror, index: usize) -> Option<SessionRow> {
        mirror.sessions.values().nth(index).map(|x| session_row(x, mirror))
    }

    /// What the browser does with `command`: [`Command::remote`], but it has no microphone for a voice's.
    fn remote_of(&self, command: &Command, op: &mut dyn FnMut() -> String) -> Remote {
        match command {
            Command::VoiceToSession { .. } => Remote::Unsupported,
            _ => command.remote(self, op),
        }
    }

    /// Whether `command` can go to the host now: connected, with mutations for it, each of them granted. The
    /// views ask with what they have in hand, a steer with no target yet or a review with no comment yet, so
    /// these ask as what they will send. A command that names what is gone cannot go.
    fn sends(&self, command: &Command) -> bool {
        if !self.live() {
            return false;
        }
        let asked;
        let command = match command {
            Command::Steer { .. } => {
                asked = Command::Steer { sessions: vec![String::new()], text: String::new() };
                &asked
            }
            Command::EditComments { root, .. } => {
                asked = Command::SendReview(root.clone());
                &asked
            }
            Command::IntegrateAccepted(plan) => {
                asked = Command::IntegrateTask { plan: *plan, task: String::new() };
                &asked
            }
            Command::Start { .. } | Command::StartAgent { .. }
                if known_agents(&self.state.borrow().mirror).is_empty() =>
            {
                return false;
            }
            command => command,
        };
        match self.remote_of(command, &mut String::new) {
            Remote::Send(mutations) => !mutations.is_empty() && mutations.iter().all(|m| self.may(m.grant())),
            Remote::Client | Remote::Unsupported => false,
        }
    }

    /// The number of `command` if it is a steer, to `targets` targets; none is 0.
    fn steering(&self, command: &Command, targets: usize) -> u64 {
        let Command::Steer { text, .. } = command else { return 0 };
        let mut s = self.state.borrow_mut();
        s.last_steer += 1;
        let steer = s.last_steer;
        let steering =
            Steering { text: text.clone(), waiting: targets, reached_any: false, missed: Vec::new(), why: None };
        s.steers.insert(steer, steering);
        steer
    }

    /// What `mutation` of `command` changes on screen while it is in flight; `steer` is the number of a steer.
    fn flight(&self, mirror: &Mirror, mutation: &ops::Command, command: &Command, steer: u64) -> Option<Flight> {
        match (mutation, command) {
            (ops::Command::ApprovalDecide(decide), Command::Decide { session, .. }) => {
                let row = self.session_row_in(mirror, *session)?;
                Some(Flight::Decision(row.id, decide.approval.clone()))
            }
            (
                ops::Command::SessionCommand(SessionCommand { action: SessionAction::Steer { session, .. }, .. }),
                Command::Steer { .. },
            ) => Some(Flight::Steer(steer, session.clone())),
            (
                ops::Command::SessionCommand(SessionCommand { action: SessionAction::Start { .. }, .. }),
                Command::Start { .. },
            ) => Some(Flight::Start),
            (
                ops::Command::ReviewCommand(ReviewCommand {
                    worktree,
                    action: ReviewAction::Add { path, line, removed, text, .. },
                    ..
                }),
                _,
            ) => {
                let comment = ReviewComment {
                    path: path.clone(),
                    line: *line,
                    removed: *removed,
                    excerpt: String::new(),
                    body: text.clone(),
                };
                Some(Flight::Comment(PathBuf::from(worktree), comment))
            }
            (
                ops::Command::PlanCommand(PlanCommand {
                    plan: Some(plan), action: PlanAction::Integrate { task }, ..
                }),
                _,
            ) => Some(Flight::Integrate(plan.clone(), task.clone())),
            _ => None,
        }
    }

    /// A command that will not be sent: an approval the banner may offer again, a steer the bar holds again, and
    /// comments their boxes hold again.
    fn refuse(&self, command: &Command) {
        let mut s = self.state.borrow_mut();
        match command {
            Command::Decide { session, approval, .. } => {
                if let Some(id) = s.mirror.sessions.values().nth(*session).map(|x| x.id.clone()) {
                    s.refused.push((id, approval.clone()));
                }
            }
            Command::Steer { text, .. } => s.unsent = Some(text.clone()),
            Command::EditComments { root, before, after } => {
                let unsent = after.iter().filter(|comment| !before.contains(comment));
                let unsent: Vec<_> = unsent.map(|comment| (root.clone(), comment.clone())).collect();
                s.unsent_comments.extend(unsent);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests;
