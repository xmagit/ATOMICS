//! A host for the tests: the Noise responder of `atomics_core::noise` and a small `atomics.v1` server over
//! one browser channel at a time, holding a board, its event log (for replay) and file diffs. It speaks
//! the protocol the real host does, as far as the web client uses it. It uses atomics-core alone, so the
//! headless-browser test (`tests/browser.rs`) serves the real bundle from it too.
//!
//! Its terminals ([`FakeTerminal`]) echo what is typed into them, as `cat` in a cooked terminal would,
//! under leases with fencing tokens and input numbers as the real host keeps them. Its own state ([`Board::host`]:
//! the workspace and the ticket sign-in) goes with its snapshots and `host_upsert`, and `host_command` changes it
//! as the real host's would, without a ticket server: a sign-in waits on [`SIGN_IN_URL`] until any address is
//! pasted.

use std::collections::{BTreeMap, HashMap};

use atomics_core::diff::FileDiff;
use atomics_core::lease::{GridSize, LeaseReason, LeaseState};
use atomics_core::noise::{Access, Channel, Kind, PairAnswer, PairRequest, Pairing, PublicKey, Responder, Secret};
use atomics_core::ops::{
    CommandResult, HostAction, HostCommand, Outcome, PlanAction, PlanCommand, ReviewAction, ReviewCommand,
    SessionAction, SessionCommand,
};
use atomics_core::projection::{
    ApprovalInfo, DiffFileBody, DiffSummary, HostInfo, PlanInfo, SessionInfo, SignInState, TaskInfo,
};
use atomics_core::screen::{HistoryChunk, Line, RowsEncoder, Screen};
use atomics_core::steer::ReviewComment;
use atomics_core::tunnel::close;
use atomics_core::wire::{
    self, CAPABILITIES, Disposition, Envelope, ErrorCode, Limits, Message, Resume, Revisions, Seq, Topic, VERSION,
    WireError,
};

pub const HOST_ID: &str = "6f1c2a7e-0b7d-4c55-9f43-3d2a1b0c9e8f";
/// The address a sign-in this host starts waits on.
pub const SIGN_IN_URL: &str = "https://auth.example.com/authorize?client_id=atomics&state=s1";

enum Stage {
    /// Nothing read on this channel yet.
    First,
    /// Pairing: waiting for the browser's `PairRequest`.
    Named(Channel),
    /// Pairing: the request is in; the host's user compares the codes ([`FakeHost::confirm`]).
    Comparing(Channel),
    Open(Channel),
    /// A session with a key this host does not hold: its first message gets the sealed refusal.
    Refusing(Channel),
}

#[derive(Default, Clone)]
pub struct Board {
    pub sessions: Vec<SessionInfo>,
    pub plans: Vec<PlanInfo>,
    pub approvals: Vec<ApprovalInfo>,
    pub diffs: Vec<DiffSummary>,
    /// The host's own state: its workspaces and the ticket sign-in.
    pub host: HostInfo,
}

pub struct FakeHost {
    secret: Secret,
    pub allowed: Vec<(PublicKey, Access)>,
    pub pairing: Option<Pairing>,
    /// Another device is pairing with [`FakeHost::pairing`]: a device answering it is told to wait.
    pub busy: bool,
    /// What a pairing's user answers: the device's access, or `None` to refuse it.
    pub answer: Option<Access>,
    /// The code this host showed for the last pairing.
    pub code: Option<String>,
    /// The device name the last pairing asked with.
    pub named: Option<String>,
    /// Wait for [`FakeHost::confirm`] before answering a pairing.
    pub hold: bool,
    /// Spoil the next handshake answer, as a broker that forges one would.
    pub corrupt_answer: bool,
    /// The code to close the channel with after the messages [`FakeHost::receive`] just returned.
    closing: Option<u16>,
    stage: Option<Stage>,
    access: Access,
    pub epoch: String,
    pub board: Board,
    /// Every event, by cursor (index + 1).
    pub events: Vec<Envelope>,
    pub files: HashMap<(String, String), FileDiff>,
    pub listings: HashMap<String, Vec<String>>,
    pub contents: HashMap<(String, String), String>,
    /// Bytes of one `diff_file` page.
    pub page_budget: usize,
    pub trace: atomics_core::trace_feed::TraceData,
    trace_revision: u64,
    follows_trace: bool,
    /// Every message the browser sent on an open channel.
    pub received: Vec<Message>,
    /// Terminals by session.
    pub terminals: BTreeMap<String, FakeTerminal>,
    /// The name the browser's leases are shown under: the name it paired with.
    pub device: String,
    tokens: u64,
    /// The next plan, review, session or host command is refused with this, as a host that cannot do it says.
    pub refuse_next: Option<WireError>,
    /// What was steered, by session: delivered to the agent, not typed under a lease.
    pub steered: Vec<(String, String)>,
    /// The reviews sent to agents, by worktree.
    pub reviews: Vec<(String, Vec<ReviewComment>)>,
    /// Plan, review, session and host commands wait unanswered, as work that takes a while does.
    pub hold_results: bool,
    /// What every `host_command` asked, in order, answered or not.
    pub host_commands: Vec<HostAction>,
    /// This host offers `host`. Without it, it is one from before `host`: its snapshots carry none of its own
    /// state, and that state's events reach the browser as `skipped`, as a newer host sends them to a client
    /// that did not ask for `host`.
    pub offers_host: bool,
    /// This host has no desktop of its own, as `atomics host`: it offers `fit`, and `terminal_fit` gives a terminal
    /// its own size.
    pub headless: bool,
    /// The channel open now negotiated `host`.
    host_negotiated: bool,
}

/// A terminal that echoes what it is typed, with its lease.
pub struct FakeTerminal {
    pub screen: Screen,
    pub generation: u64,
    pub revision: u64,
    /// The terminal's own size, the desktop's (or with no desktop, the client's pane's), which it returns to when a
    /// holder's size ends.
    pub size: (u16, u16),
    pub lease: Option<FakeLease>,
    pub reason: Option<LeaseReason>,
    /// The browser follows its screen.
    pub followed: bool,
    /// Everything typed into it, in order.
    pub typed: String,
    /// Lines that scrolled off the top, oldest first: IDs `first_line - history.len() .. first_line`.
    pub history: Vec<Line>,
    /// While set, input is refused as `slow_client`: the program does not read.
    pub stalled: bool,
}

pub struct FakeLease {
    /// Who holds it: the browser, or another client by name.
    pub holder: String,
    pub browser: bool,
    pub token: u64,
    pub next: u64,
    pub sized: bool,
}

impl FakeTerminal {
    pub fn new(cols: u16, rows: u16, prompt: &str) -> Self {
        // A dark terminal's colours: xterm's 256, then foreground, background and cursor.
        let mut screen = Screen::blank(cols, rows);
        for (slot, rgb) in screen.palette.iter_mut().enumerate() {
            let color = match slot {
                256 => [220, 216, 208],
                257 => [24, 23, 22],
                258 => [217, 119, 87],
                i => atomics_core::theme::xterm_color(i).unwrap_or([128, 128, 128]),
            };
            *rgb = atomics_core::screen::Rgb(color);
        }
        let mut terminal = Self {
            screen,
            generation: 1,
            revision: 1,
            size: (cols, rows),
            lease: None,
            reason: None,
            followed: false,
            typed: String::new(),
            history: Vec::new(),
            stalled: false,
        };
        terminal.write(prompt);
        terminal
    }

    /// What the program writes: text, with `\r` (or `\n`) ending a line, DEL erasing one cell and escape
    /// sequences left out. Text past the right edge overwrites the last column, where the cursor stays: a
    /// published cursor is on the screen.
    pub fn write(&mut self, text: &str) {
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            let cursor = &mut self.screen.cursor;
            match c {
                '\r' | '\n' => {
                    cursor.col = 0;
                    if cursor.row + 1 < self.screen.rows {
                        cursor.row += 1;
                    } else {
                        let top = self.screen.lines.remove(0);
                        self.history.push(top);
                        self.screen.lines.push(Line::blank(self.screen.cols));
                        self.screen.first_line += 1;
                        self.screen.history += 1;
                    }
                }
                '\x7f' => {
                    if cursor.col > 0 {
                        cursor.col -= 1;
                        let (row, col) = (usize::from(cursor.row), usize::from(cursor.col));
                        self.screen.lines[row].cells[col].c = ' ';
                    }
                }
                '\x1b' => {
                    // A CSI or SS3 sequence: skip to its final byte.
                    if matches!(chars.peek(), Some('[' | 'O')) {
                        chars.next();
                        while chars.next().is_some_and(|c| !('@'..='~').contains(&c) || c == '[') {}
                    }
                }
                c if c < ' ' => {}
                c => {
                    let (row, col) = (usize::from(cursor.row), usize::from(cursor.col));
                    if col < usize::from(self.screen.cols) {
                        self.screen.lines[row].cells[col].c = c;
                        cursor.col = (cursor.col + 1).min(self.screen.cols - 1);
                    }
                }
            }
        }
    }

    fn snapshot(&self, session: &str) -> Vec<Message> {
        let parts = self.screen.snapshot(session, self.generation, self.revision, Some).expect("a small screen fits");
        parts.into_iter().map(Message::ScreenSnapshot).collect()
    }

    /// Change the screen with `change` and return the delta the browser follows it by.
    fn change(&mut self, session: &str, change: impl FnOnce(&mut Self)) -> Vec<Message> {
        let before = self.screen.clone();
        change(self);
        let delta = before.delta(&self.screen, session, self.generation, self.revision, self.revision + 1);
        let Some(delta) = delta else { return Vec::new() };
        self.revision += 1;
        if self.followed { vec![Message::ScreenDelta(delta)] } else { Vec::new() }
    }

    /// A new size starts a new generation, sent as a snapshot.
    fn set_size(&mut self, session: &str, (cols, rows): (u16, u16)) -> Vec<Message> {
        if (cols, rows) == (self.screen.cols, self.screen.rows) {
            return Vec::new();
        }
        let mut screen = Screen::blank(cols, rows);
        screen.palette = self.screen.palette.clone();
        for (to, from) in screen.lines.iter_mut().zip(&self.screen.lines) {
            for (cell, old) in to.cells.iter_mut().zip(&from.cells) {
                *cell = old.clone();
            }
        }
        screen.cursor = self.screen.cursor;
        screen.cursor.row = screen.cursor.row.min(rows - 1);
        screen.cursor.col = screen.cursor.col.min(cols - 1);
        screen.first_line = self.screen.first_line + 1000;
        self.screen = screen;
        self.history.clear();
        self.generation += 1;
        self.revision = 1;
        if self.followed { self.snapshot(session) } else { Vec::new() }
    }

    fn view(&self, session: &str) -> LeaseState {
        let lease = self.lease.as_ref();
        let mine = lease.filter(|l| l.browser);
        LeaseState {
            session: session.to_string(),
            holder: lease.map(|l| l.holder.clone()),
            yours: mine.is_some(),
            lease: mine.map(|l| Seq(l.token)),
            next: mine.map(|l| Seq(l.next)),
            ttl_ms: mine.map(|_| 30_000),
            resized: lease.is_some_and(|l| l.sized),
            reason: self.reason,
        }
    }

    /// The lease ends for `reason`, and the holder's size with it.
    fn end(&mut self, session: &str, reason: LeaseReason) -> Vec<Message> {
        let Some(lease) = self.lease.take() else { return Vec::new() };
        self.reason = Some(reason);
        let mut out = Vec::new();
        if lease.sized {
            out.extend(self.set_size(session, self.size));
        }
        if self.followed {
            out.push(Message::LeaseState(self.view(session)));
        }
        out
    }
}

impl FakeHost {
    pub fn new(board: Board) -> Self {
        Self {
            secret: Secret::generate().unwrap(),
            allowed: Vec::new(),
            pairing: None,
            busy: false,
            answer: Some(Access::Control),
            code: None,
            named: None,
            hold: false,
            corrupt_answer: false,
            closing: None,
            stage: None,
            access: Access::View,
            epoch: "epoch-1".into(),
            board,
            events: Vec::new(),
            files: HashMap::new(),
            listings: HashMap::new(),
            contents: HashMap::new(),
            page_budget: wire::MAX_FRAME_BYTES,
            trace: Default::default(),
            trace_revision: 1,
            follows_trace: false,
            received: Vec::new(),
            terminals: BTreeMap::new(),
            device: "Safari on iPhone".into(),
            tokens: 0,
            refuse_next: None,
            steered: Vec::new(),
            reviews: Vec::new(),
            hold_results: false,
            host_commands: Vec::new(),
            offers_host: true,
            headless: false,
            host_negotiated: false,
        }
    }

    fn send(&mut self, messages: Vec<Message>) -> Vec<Vec<u8>> {
        let envelopes: Vec<Envelope> = messages.into_iter().map(|m| Envelope::host(&self.epoch, m)).collect();
        match &mut self.stage {
            Some(Stage::Open(channel)) => Self::seal(channel, &envelopes),
            _ => Vec::new(),
        }
    }

    /// Someone types on the desktop: a lease on `session`'s terminal ends, as the real host's does.
    pub fn desktop_types(&mut self, session: &str, text: &str) -> Vec<Vec<u8>> {
        let terminal = self.terminals.get_mut(session).expect("a terminal");
        let mut out = terminal.end(session, LeaseReason::Desktop);
        out.extend(terminal.change(session, |t| {
            t.typed.push_str(text);
            t.write(text);
        }));
        self.send(out)
    }

    /// Another client takes `session`'s lease over, as `name`.
    pub fn other_takes(&mut self, session: &str, name: &str) -> Vec<Vec<u8>> {
        self.tokens += 1;
        let terminal = self.terminals.get_mut(session).expect("a terminal");
        let mut out = Vec::new();
        if terminal.lease.as_ref().is_some_and(|l| l.sized) {
            out.extend(terminal.set_size(session, terminal.size));
        }
        let reason = if terminal.lease.is_some() { LeaseReason::TakenOver } else { LeaseReason::Granted };
        terminal.lease =
            Some(FakeLease { holder: name.into(), browser: false, token: self.tokens, next: 1, sized: false });
        terminal.reason = Some(reason);
        if terminal.followed {
            out.push(Message::LeaseState(terminal.view(session)));
        }
        self.send(out)
    }

    /// The program in `session` sets its modes.
    pub fn modes(&mut self, session: &str, set: impl FnOnce(&mut atomics_core::screen::Modes)) -> Vec<Vec<u8>> {
        let terminal = self.terminals.get_mut(session).expect("a terminal");
        let out = terminal.change(session, |t| set(&mut t.screen.modes));
        self.send(out)
    }

    /// The program in `session` prints `text`.
    pub fn output(&mut self, session: &str, text: &str) -> Vec<Vec<u8>> {
        let terminal = self.terminals.get_mut(session).expect("a terminal");
        let out = terminal.change(session, |t| t.write(text));
        self.send(out)
    }

    pub fn publish_trace(&mut self, data: atomics_core::trace_feed::TraceData) -> Vec<Vec<u8>> {
        data.validate().unwrap();
        self.trace = data;
        self.trace_revision += 1;
        if self.follows_trace {
            self.send(vec![Message::TraceChanged { revision: Seq(self.trace_revision) }])
        } else {
            Vec::new()
        }
    }

    pub fn public(&self) -> PublicKey {
        self.secret.public()
    }

    /// Offer a pairing; its link goes into the boot.
    pub fn open_pairing(&mut self) -> Pairing {
        let pairing = Pairing::new(HOST_ID, self.public()).unwrap();
        self.pairing = Some(pairing.clone());
        pairing
    }

    pub fn head(&self) -> u64 {
        self.events.len() as u64
    }

    pub fn join(&mut self) {
        self.stage = Some(Stage::First);
        self.closing = None;
    }

    /// The browser's channel ended: it follows nothing, and its leases end with it.
    pub fn left(&mut self) {
        self.stage = None;
        for (session, terminal) in &mut self.terminals {
            terminal.followed = false;
            if terminal.lease.as_ref().is_some_and(|l| l.browser) {
                terminal.end(session, LeaseReason::Disconnected);
            }
        }
    }

    fn seal(channel: &mut Channel, envelopes: &[Envelope]) -> Vec<Vec<u8>> {
        envelopes.iter().flat_map(|e| channel.seal(wire::encode(e).unwrap().as_bytes()).unwrap()).collect()
    }

    /// Record an event at the next cursor, change the board with it, and return what an open channel sends.
    pub fn event(&mut self, message: impl FnOnce(Seq) -> Message) -> Vec<Vec<u8>> {
        let message = message(Seq(self.head() + 1));
        match &message {
            Message::SessionUpsert { session, .. } => {
                self.board.sessions.retain(|s| s.id != session.id);
                self.board.sessions.push(session.clone());
            }
            Message::SessionRemoved { id, .. } => self.board.sessions.retain(|s| s.id != *id),
            Message::PlanUpsert { plan, .. } => {
                self.board.plans.retain(|p| p.id != plan.id);
                self.board.plans.push(plan.clone());
            }
            Message::ApprovalPending { approval, .. } => self.board.approvals.push(approval.clone()),
            Message::ApprovalResolved { id, .. } => self.board.approvals.retain(|a| a.id != *id),
            Message::DiffSummary { summary, .. } => {
                self.board.diffs.retain(|d| d.worktree != summary.worktree);
                self.board.diffs.push(summary.clone());
            }
            Message::HostUpsert { host, .. } => self.board.host = host.clone(),
            _ => {}
        }
        let envelope = Envelope::host(&self.epoch, message);
        self.events.push(envelope.clone());
        let sent = self.for_channel(&envelope);
        match &mut self.stage {
            Some(Stage::Open(channel)) => Self::seal(channel, &[sent]),
            _ => Vec::new(),
        }
    }

    /// The host's own state changes to `host`: the `host_upsert` an open channel is sent.
    pub fn publish_host(&mut self, host: HostInfo) -> Vec<Vec<u8>> {
        self.event(|cursor| Message::HostUpsert { cursor, host })
    }

    /// An event as the open channel is sent it: one of the host's own state as `skipped` without `host`.
    fn for_channel(&self, event: &Envelope) -> Envelope {
        match &event.message {
            Message::HostUpsert { cursor, .. } if !self.host_negotiated => {
                Envelope::host(&self.epoch, Message::Skipped { cursor: *cursor })
            }
            _ => event.clone(),
        }
    }

    /// An event's frame without recording it: a gap or a duplicate, for the client to notice.
    pub fn stray(&mut self, message: Message) -> Vec<Vec<u8>> {
        let envelope = Envelope::host(&self.epoch, message);
        match &mut self.stage {
            Some(Stage::Open(channel)) => Self::seal(channel, &[envelope]),
            _ => Vec::new(),
        }
    }

    fn snapshot(&self) -> Envelope {
        let rev = Seq(self.head());
        Envelope::host(
            &self.epoch,
            Message::StateSnapshot {
                cursor: Seq(self.head()),
                revisions: Revisions { sessions: rev, plans: rev, approvals: rev, diffs: rev, host: rev },
                sessions: self.board.sessions.clone(),
                plans: self.board.plans.clone(),
                approvals: self.board.approvals.clone(),
                diffs: self.board.diffs.clone(),
                host: if self.offers_host { self.board.host.clone() } else { HostInfo::default() },
                part: None,
            },
        )
    }

    /// One binary message from the browser: the messages to answer with, or the code the channel closes with.
    pub fn receive(&mut self, message: &[u8]) -> Result<Vec<Vec<u8>>, u16> {
        match self.stage.take() {
            None => Err(close::REFUSED),
            Some(Stage::First) => {
                let kind = Kind::of(message).map_err(|_| close::REFUSED)?;
                let psk = match kind {
                    Kind::Pair => {
                        let pairing = self.pairing.as_ref().ok_or(close::NO_PAIRING)?;
                        if self.busy {
                            return Err(close::PAIRING_BUSY);
                        }
                        Some(pairing.psk().clone())
                    }
                    Kind::Session => None,
                };
                let responder =
                    Responder::read(message, HOST_ID, &self.secret, psk.as_ref()).map_err(|_| close::REFUSED)?;
                let device = responder.device();
                let known = self.allowed.iter().find(|(key, _)| *key == device).map(|(_, access)| *access);
                let (mut answer, done) = responder.answer().map_err(|_| close::REFUSED)?;
                if std::mem::take(&mut self.corrupt_answer) {
                    *answer.last_mut().expect("an answer") ^= 1;
                }
                self.stage = Some(match (kind, known) {
                    (Kind::Pair, _) => {
                        self.code = Some(done.code);
                        self.pairing = None; // single use
                        self.access = self.answer.unwrap_or(Access::View);
                        self.allowed.push((device, self.access));
                        Stage::Named(done.channel)
                    }
                    (Kind::Session, Some(access)) => {
                        self.access = access;
                        Stage::Open(done.channel)
                    }
                    (Kind::Session, None) => Stage::Refusing(done.channel),
                });
                Ok(vec![answer])
            }
            Some(Stage::Named(mut channel)) => {
                let frame = channel.open(message).map_err(|_| close::REFUSED)?.ok_or(close::REFUSED)?;
                let request: PairRequest = serde_json::from_slice(&frame).map_err(|_| close::REFUSED)?;
                self.device.clone_from(&request.name);
                self.named = Some(request.name);
                self.stage = Some(Stage::Comparing(channel));
                Ok(if self.hold { Vec::new() } else { self.confirm() })
            }
            Some(Stage::Comparing(channel)) => {
                self.stage = Some(Stage::Comparing(channel));
                Err(close::REFUSED)
            }
            // Its first message decrypted: the refusal, sealed, then `not_paired`.
            Some(Stage::Refusing(mut channel)) => {
                if channel.open(message).map_err(|_| close::REFUSED)?.is_none() {
                    self.stage = Some(Stage::Refusing(channel));
                    return Ok(Vec::new());
                }
                self.closing = Some(close::NOT_PAIRED);
                Ok(Self::seal(&mut channel, &[wire::not_paired()]))
            }
            Some(Stage::Open(mut channel)) => {
                let Some(frame) = channel.open(message).map_err(|_| close::REFUSED)? else {
                    self.stage = Some(Stage::Open(channel));
                    return Ok(Vec::new());
                };
                let envelope = wire::decode(std::str::from_utf8(&frame).unwrap()).map_err(|_| close::REFUSED)?;
                self.received.push(envelope.message.clone());
                let replies = self.answer_frame(envelope);
                let sealed = Self::seal(&mut channel, &replies);
                self.stage = Some(Stage::Open(channel));
                Ok(sealed)
            }
        }
    }

    /// The code the host closes the channel with once the messages [`FakeHost::receive`] just
    /// returned are sent, if it does.
    pub fn take_close(&mut self) -> Option<u16> {
        self.closing.take()
    }

    /// The host's user answers the pairing with [`FakeHost::answer`]: the messages to send.
    pub fn confirm(&mut self) -> Vec<Vec<u8>> {
        let Some(Stage::Comparing(mut channel)) = self.stage.take() else { return Vec::new() };
        let answer = PairAnswer { paired: self.answer.is_some(), may: self.answer };
        let reply = channel.seal(&serde_json::to_vec(&answer).unwrap()).unwrap();
        if self.answer.is_some() {
            self.stage = Some(Stage::Open(channel));
        } else {
            self.allowed.pop();
        }
        reply
    }

    fn answer_frame(&mut self, envelope: Envelope) -> Vec<Envelope> {
        let id = envelope.request_id.clone();
        let epoch = self.epoch.clone();
        let host = move |m| Envelope::host(&epoch, m);
        match envelope.message {
            Message::Hello { capabilities, prior_epoch, prior_cursor, .. } => {
                let offered: Vec<&str> = CAPABILITIES
                    .iter()
                    .copied()
                    .filter(|c| (self.offers_host || *c != wire::HOST) && (self.headless || *c != wire::FIT))
                    .collect();
                let capabilities = wire::common_capabilities(&capabilities, &offered);
                self.host_negotiated = capabilities.iter().any(|c| c == wire::HOST);
                self.follows_trace = false;
                let role = self.access.role();
                let resume =
                    wire::decide_resume(&self.epoch, Seq(self.head()), Seq(1), prior_epoch.as_deref(), prior_cursor);
                let may = role.may(&capabilities);
                let welcome = host(Message::Welcome {
                    version: VERSION,
                    capabilities,
                    limits: Limits::default(),
                    role,
                    resume,
                    may,
                });
                let mut out = vec![welcome];
                match resume {
                    Resume::Reset { .. } => out.push(self.snapshot()),
                    Resume::Replay { after } => {
                        out.extend(self.events.iter().skip(after.0 as usize).map(|e| self.for_channel(e)));
                    }
                }
                out
            }
            Message::Resync { screens, .. } if !screens.is_empty() => screens
                .iter()
                .filter_map(|s| self.terminals.get(s).filter(|t| t.followed).map(|t| t.snapshot(s)))
                .flatten()
                .map(host)
                .collect(),
            Message::Resync { .. } => vec![self.snapshot()],
            message @ (Message::Subscribe { .. }
            | Message::Unsubscribe { .. }
            | Message::LeaseAcquire { .. }
            | Message::LeaseRenew { .. }
            | Message::LeaseRelease { .. }
            | Message::TerminalInput { .. }
            | Message::TerminalResize { .. }
            | Message::TerminalFit { .. }
            | Message::HistoryGet(_)) => self.terminal_frame(message, id),
            Message::Ping { nonce } => vec![host(Message::Pong { nonce }).reply_to(id)],
            Message::FolderGet { worktree, path, revision, offset } => {
                match self.folder_page(&worktree, &path, offset) {
                    Ok(page) if page.revision == revision => vec![host(Message::FolderPage(page)).reply_to(id)],
                    Ok(_) => vec![
                        WireError::new(ErrorCode::StaleRevision, "folder listing changed")
                            .into_envelope(id, Some(self.epoch.clone())),
                    ],
                    Err(error) => vec![error.into_envelope(id, Some(self.epoch.clone()))],
                }
            }
            Message::FileGet { worktree, path } => match self.contents.get(&(worktree.clone(), path.clone())) {
                Some(content) => vec![
                    host(Message::FileContent { worktree, path, content: content.clone(), truncated: false })
                        .reply_to(id),
                ],
                None => vec![
                    WireError::new(ErrorCode::NotFound, "no such worktree file")
                        .into_envelope(id, Some(self.epoch.clone())),
                ],
            },
            Message::TraceGet { revision, offset, limit } => {
                if revision.0 != self.trace_revision {
                    return vec![
                        WireError::new(ErrorCode::StaleRevision, "trace changed")
                            .into_envelope(id, Some(self.epoch.clone())),
                    ];
                }
                match self.trace.page(self.trace_revision, offset, limit, self.page_budget) {
                    Ok(page) => vec![host(Message::TracePage(page)).reply_to(id)],
                    Err(error) => vec![error.into_envelope(id, Some(self.epoch.clone()))],
                }
            }
            Message::DiffGet { worktree, path, revision, offset } => {
                let summary = self.board.diffs.iter().find(|d| d.worktree == worktree);
                let file = self.files.get(&(worktree.clone(), path.clone()));
                let (Some(summary), Some(file)) = (summary, file) else {
                    return vec![
                        WireError::new(ErrorCode::NotFound, "no such file").into_envelope(id, Some(self.epoch.clone())),
                    ];
                };
                if revision.is_some_and(|r| r.0 != summary.revision) {
                    return vec![
                        WireError::new(ErrorCode::StaleRevision, "the diff moved on")
                            .into_envelope(id, Some(self.epoch.clone())),
                    ];
                }
                let (page, next) = wire::diff_page(file, offset.unwrap_or(0) as usize, self.page_budget).unwrap();
                let body = DiffFileBody { worktree, revision: summary.revision, file: page };
                vec![
                    host(Message::DiffFile { body, truncated: next.is_some(), next_offset: next.map(|n| n as u32) })
                        .reply_to(id),
                ]
            }
            Message::ApprovalDecide(decide) => {
                let mut out = Vec::new();
                let outcome = if self.board.approvals.iter().any(|a| a.id == decide.approval) {
                    let approval = decide.approval.clone();
                    let _ = self.event(|cursor| Message::ApprovalResolved {
                        cursor,
                        id: approval,
                        disposition: Disposition::Allowed,
                    });
                    out.push(self.events.last().cloned().expect("just recorded"));
                    Outcome::done(serde_json::Value::Null)
                } else {
                    Outcome::refused(WireError::new(ErrorCode::AlreadyResolved, "answered on the desktop"))
                };
                out.push(host(Message::CommandResult(CommandResult { op: decide.op, outcome })));
                out
            }
            Message::PlanCommand(command) => {
                let op = command.op.clone();
                self.mutate(op, |host| host.plan_command(command))
            }
            Message::ReviewCommand(command) => {
                let op = command.op.clone();
                self.mutate(op, |host| host.review_command(command))
            }
            Message::SessionCommand(command) => {
                let op = command.op.clone();
                self.mutate(op, |host| host.session_command(command))
            }
            Message::HostCommand(command) => {
                let op = command.op.clone();
                self.host_commands.push(command.action.clone());
                self.mutate(op, |host| host.host_command(command))
            }
            _ => Vec::new(),
        }
    }

    /// Carry out a mutation, unless [`FakeHost::refuse_next`] says otherwise: the events it made, then its result.
    fn mutate(&mut self, op: String, carry_out: impl FnOnce(&mut Self) -> Outcome) -> Vec<Envelope> {
        if self.hold_results {
            return Vec::new();
        }
        let before = self.events.len();
        let outcome = match self.refuse_next.take() {
            Some(error) => Outcome::refused(error),
            None => carry_out(self),
        };
        let mut out = self.events[before..].to_vec();
        out.push(Envelope::host(&self.epoch, Message::CommandResult(CommandResult { op, outcome })));
        out
    }

    fn plan_command(&mut self, command: PlanCommand) -> Outcome {
        use atomics_core::plan::{Plan, PlanStatus, TaskStatus};
        let refused = |code, text: &str| Outcome::refused(WireError::new(code, text));
        let Some(mut plan) = self.board.plans.iter().find(|p| Some(&p.id) == command.plan.as_ref()).cloned() else {
            return refused(ErrorCode::NotFound, "no such plan");
        };
        let branch = Plan { id: plan.id.clone(), integration: plan.integration.clone(), ..Plan::default() };
        let done = |plan: &atomics_core::projection::PlanInfo, id: &String| {
            plan.tasks.iter().any(|t| t.id == *id && t.status == TaskStatus::Done)
        };
        match command.action {
            PlanAction::Approve if command.digest.as_deref() == Some(plan.digest.as_str()) => plan.approved = true,
            PlanAction::Approve => return refused(ErrorCode::StaleRevision, "the plan changed"),
            PlanAction::Dispatch { .. } if !plan.approved => return refused(ErrorCode::Rejected, "not approved yet"),
            PlanAction::Dispatch { task } => {
                let snapshot = plan.clone();
                let ready = plan.tasks.iter_mut().filter(|t| {
                    t.status == TaskStatus::Planned
                        && t.depends_on.iter().all(|dep| done(&snapshot, dep))
                        && task.as_ref().is_none_or(|id| *id == t.id)
                });
                let mut started = 0;
                for t in ready {
                    t.status = TaskStatus::Running;
                    started += 1;
                }
                if started == 0 {
                    return refused(ErrorCode::Rejected, "no task is ready");
                }
            }
            PlanAction::Update { task, status, .. } => {
                let Some(found) = plan.tasks.iter_mut().find(|t| t.id == task) else {
                    return refused(ErrorCode::NotFound, "no such task");
                };
                if let Some(status) = status {
                    found.status = status;
                }
            }
            PlanAction::Integrate { task } => {
                let Some(found) = plan.tasks.iter_mut().find(|t| t.id == task) else {
                    return refused(ErrorCode::NotFound, "no such task");
                };
                found.integrated = Some(branch.integration_branch());
            }
            PlanAction::Archive => plan.status = PlanStatus::Archived,
            PlanAction::Complete => plan.status = PlanStatus::Completed,
            PlanAction::Set { .. } => return refused(ErrorCode::Rejected, "not in this fake"),
        }
        let _ = self.event(|cursor| Message::PlanUpsert { cursor, plan });
        Outcome::done(serde_json::Value::Null)
    }

    fn review_command(&mut self, command: ReviewCommand) -> Outcome {
        let refused = |code, text: String| Outcome::refused(WireError::new(code, text));
        let Some(mut summary) = self.board.diffs.iter().find(|d| d.worktree == command.worktree).cloned() else {
            return refused(ErrorCode::NotFound, "no diff for that worktree".into());
        };
        let mut result = serde_json::Value::Null;
        match command.action {
            ReviewAction::Add { revision, .. } if revision.0 != summary.revision => {
                let text = format!("the diff is at revision {}; anchor the comment again", summary.revision);
                return refused(ErrorCode::StaleRevision, text);
            }
            ReviewAction::Add { path, line, removed, text, .. } => {
                summary.comments.push(ReviewComment { path, line, removed, excerpt: String::new(), body: text });
            }
            ReviewAction::Remove { path, line, removed, text } => {
                let at = summary
                    .comments
                    .iter()
                    .position(|c| c.path == path && c.line == line && c.removed == removed && c.body == text);
                let Some(at) = at else { return refused(ErrorCode::NotFound, "no such comment".into()) };
                summary.comments.remove(at);
            }
            ReviewAction::Send => {
                if summary.comments.is_empty() {
                    return refused(ErrorCode::Rejected, "no pending comments".into());
                }
                result = serde_json::json!({ "sent_comments": summary.comments.len() });
                self.reviews.push((summary.worktree.clone(), std::mem::take(&mut summary.comments)));
            }
        }
        let _ = self.event(|cursor| Message::DiffSummary { cursor, summary });
        Outcome::done(result)
    }

    fn session_command(&mut self, command: SessionCommand) -> Outcome {
        use atomics_core::session::AgentStatus;
        let not_found = |id: &str| Outcome::refused(WireError::new(ErrorCode::NotFound, format!("no session {id}")));
        match command.action {
            SessionAction::Steer { session, .. } if !self.board.sessions.iter().any(|s| s.id == session) => {
                not_found(&session)
            }
            SessionAction::Steer { session, text } => {
                self.steered.push((session, text));
                Outcome::done(serde_json::json!({ "sent": true, "queued_until_next_hook": false }))
            }
            SessionAction::Start { agent, folder, .. } => {
                let id = format!("new-{}", self.head());
                let session = SessionInfo {
                    id: id.clone(),
                    title: "new session".into(),
                    agent,
                    cwd: folder.unwrap_or_else(|| "/scratch/new".into()),
                    worktree: None,
                    branch: None,
                    status: AgentStatus::Idle,
                    chief: false,
                    task: None,
                    pending_approvals: 0,
                };
                self.terminals.insert(id.clone(), FakeTerminal::new(80, 12, "$ "));
                let _ = self.event(|cursor| Message::SessionUpsert { cursor, session });
                Outcome::done(serde_json::json!({ "id": id }))
            }
            SessionAction::Close { session } if !self.board.sessions.iter().any(|s| s.id == session) => {
                not_found(&session)
            }
            SessionAction::Close { session } => {
                self.terminals.remove(&session);
                let _ = self.event(|cursor| Message::SessionRemoved { cursor, id: session });
                Outcome::done(serde_json::Value::Null)
            }
        }
    }

    /// The sign-in and the workspace, as the real host changes them.
    fn host_command(&mut self, command: HostCommand) -> Outcome {
        let refused = |code, text: String| Outcome::refused(WireError::new(code, text));
        let mut host = self.board.host.clone();
        let now = host.sign_in.as_ref().map(|sign_in| sign_in.state);
        let (state, url) = match command.action {
            HostAction::Tickets { request, ticket } => {
                use atomics_core::ticket_ui::{TicketAction, TicketReply};
                let Some(info) = host.tickets.as_mut() else {
                    return refused(ErrorCode::Rejected, "tickets unavailable".into());
                };
                let mut reply = TicketReply::new(request);
                match *ticket {
                    TicketAction::Search { cursor, .. } => {
                        reply.cards = vec![ticket_card(if cursor.is_some() { "ENG-124" } else { "ENG-123" })];
                        reply.next = cursor.is_none().then(|| "next-page".into());
                    }
                    TicketAction::Read { ticket } => reply.cards = vec![ticket_card(ticket.label())],
                    TicketAction::Children { .. } => reply.cards = vec![ticket_card("ENG-125")],
                    TicketAction::Related { .. } => reply.cards = vec![ticket_card("ENG-126")],
                    TicketAction::Plan { ticket, .. } => {
                        reply.plan = self.board.plans.first().map(|plan| plan.id.clone());
                        if let Some(plan) = self.board.plans.first_mut() {
                            plan.ticket = Some(ticket.label().into());
                        }
                    }
                    TicketAction::Link { plan, ticket, .. } => {
                        let Some(plan) = self.board.plans.iter_mut().find(|p| p.id == plan) else {
                            return refused(ErrorCode::NotFound, format!("no plan {plan}"));
                        };
                        plan.ticket = Some(ticket.label().into());
                    }
                }
                info.replies.push(reply);
                let _ = self.publish_host(host);
                return Outcome::done(serde_json::Value::Null);
            }

            HostAction::Edit { root, path, .. } => {
                if !self.board.sessions.iter().any(|s| s.cwd == root || s.worktree.as_deref() == Some(&root)) {
                    return refused(ErrorCode::NotFound, "no worktree at that root".into());
                }
                let terminal = atomics_core::projection::editor_terminal(&root);
                self.terminals.entry(terminal.clone()).or_insert_with(|| FakeTerminal::new(80, 12, "Helix editor\r\n"));
                host.editors.retain(|e| e.root != root);
                host.editors.push(atomics_core::projection::EditorInfo {
                    root,
                    terminal,
                    file: path,
                    running: true,
                    inlay_hints: true,
                });
                let _ = self.publish_host(host);
                return Outcome::done(serde_json::Value::Null);
            }
            HostAction::EditorHints { root } => {
                return if host.editors.iter().any(|e| e.root == root && e.running) {
                    Outcome::done(serde_json::Value::Null)
                } else {
                    refused(ErrorCode::NotFound, "no running editor".into())
                };
            }
            HostAction::PullRequestDiff { key } => {
                return if host.pull_requests.iter().any(|p| p.key == key) {
                    Outcome::done(serde_json::Value::Null)
                } else {
                    refused(ErrorCode::NotFound, "no pull request".into())
                };
            }
            HostAction::StartChief => {
                if !self.board.sessions.iter().any(|s| s.chief) {
                    let chief = board().sessions.into_iter().find(|s| s.chief).unwrap();
                    self.terminals.insert(chief.id.clone(), FakeTerminal::new(80, 12, "chief> "));
                    let _ = self.event(|cursor| Message::SessionUpsert { cursor, session: chief });
                }
                return Outcome::done(serde_json::Value::Null);
            }
            HostAction::OpenWorkspace { name } if !host.workspaces.contains(&name) => {
                return refused(ErrorCode::NotFound, format!("no workspace {name}"));
            }
            HostAction::OpenWorkspace { name } => {
                host.workspace = Some(name);
                let _ = self.publish_host(host);
                return Outcome::done(serde_json::Value::Null);
            }
            HostAction::BrowseFolder { path } => {
                let Some(info) = host.management.as_mut() else {
                    return refused(ErrorCode::Rejected, "no management".into());
                };
                info.folders = vec![atomics_core::projection::FolderInfo {
                    requested: path.clone(),
                    parent: (path != "/home/user").then(|| "/home/user".into()),
                    project: path == "/home/user/repo",
                    worktree: (path == "/home/user/repo").then(|| path.clone()),
                    page: atomics_core::folder::entries_page(
                        if path == "/home/user" {
                            vec![atomics_core::folder::Entry { path: "repo".into(), directory: true }]
                        } else {
                            vec![]
                        },
                        &path,
                        "",
                        0,
                        0,
                        8192,
                    )
                    .unwrap(),
                }];
                let _ = self.publish_host(host);
                return Outcome::done(serde_json::Value::Null);
            }
            HostAction::OpenProject { path } => {
                if !host.projects.iter().any(|project| project.root == path) {
                    host.projects.push(atomics_core::projection::ProjectInfo { root: path, worktrees: Vec::new() });
                }
                let _ = self.publish_host(host);
                return Outcome::done(serde_json::Value::Null);
            }
            HostAction::PairDevice => {
                if let Some(info) = host.management.as_mut() {
                    info.remote.pairing = Some(atomics_core::projection::PairingInfo::Open {
                        link: Some("https://example.com/pair#single-use".into()),
                        seconds_left: 300,
                        busy: false,
                    });
                }
                let _ = self.publish_host(host);
                return Outcome::done(serde_json::Value::Null);
            }
            HostAction::CancelPairing => {
                if let Some(info) = host.management.as_mut() {
                    info.remote.pairing = Some(atomics_core::projection::PairingInfo::Idle { note: None });
                }
                let _ = self.publish_host(host);
                return Outcome::done(serde_json::Value::Null);
            }
            HostAction::SettingsTool { .. } | HostAction::SelectTheme { .. } | HostAction::RevokeDevice { .. } => {
                return Outcome::done(serde_json::Value::Null);
            }
            _ if now.is_none() => return refused(ErrorCode::Rejected, "the ticket system has no sign-in".into()),
            HostAction::SignIn => (SignInState::SigningIn, Some(SIGN_IN_URL.to_string())),
            HostAction::SignInPaste { .. } if now != Some(SignInState::SigningIn) => {
                return refused(ErrorCode::Rejected, "no sign-in is under way".into());
            }
            HostAction::SignInPaste { .. } => (SignInState::SignedIn, None),
            HostAction::SignInCancel => (SignInState::SignedOut, None),
            _ => return refused(ErrorCode::Rejected, "management fixture has no handler".into()),
        };
        if let Some(sign_in) = host.sign_in.as_mut() {
            (sign_in.state, sign_in.url, sign_in.rejected) = (state, url, None);
        }
        let _ = self.publish_host(host);
        Outcome::done(serde_json::Value::Null)
    }

    fn terminal_frame(&mut self, message: Message, id: Option<String>) -> Vec<Envelope> {
        let epoch = self.epoch.clone();
        let refuse = |code: ErrorCode, text: &str| {
            vec![WireError::new(code, text).into_envelope(id.clone(), Some(epoch.clone()))]
        };
        let host = |m: Message| Envelope::host(&epoch, m);
        let device = self.device.clone();
        match message {
            Message::Subscribe { topics } => {
                // As the host: a screen with no terminal (yet) is refused, the request named.
                let missing = topics.iter().any(
                    |topic| matches!(topic, Topic::Screen { session, .. } if !self.terminals.contains_key(session)),
                );
                if missing {
                    return refuse(ErrorCode::NotFound, "no terminal");
                }
                let mut out = Vec::new();
                if topics.contains(&Topic::Trace) {
                    self.follows_trace = true;
                    out.push(Message::TraceChanged { revision: Seq(self.trace_revision) });
                }
                for topic in topics {
                    if let Topic::Folder { worktree, path } = &topic {
                        match self.folder_page(worktree, path, 0) {
                            Ok(page) => out.push(Message::FolderPage(page)),
                            Err(error) => return vec![error.into_envelope(id, Some(self.epoch.clone()))],
                        }
                    }
                    let Topic::Screen { session, .. } = topic else { continue };
                    let Some(terminal) = self.terminals.get_mut(&session) else { continue };
                    if !terminal.followed {
                        terminal.followed = true;
                        out.extend(terminal.snapshot(&session));
                        out.push(Message::LeaseState(terminal.view(&session)));
                    }
                }
                out.into_iter().map(host).collect()
            }
            Message::Unsubscribe { topics } => {
                if topics.contains(&Topic::Trace) {
                    self.follows_trace = false;
                }
                // Unsubscribing ends this client's lease; it no longer follows, so it is told nothing.
                for topic in topics {
                    let Topic::Screen { session, .. } = topic else { continue };
                    let Some(terminal) = self.terminals.get_mut(&session) else { continue };
                    terminal.followed = false;
                    if terminal.lease.as_ref().is_some_and(|l| l.browser) {
                        terminal.end(&session, LeaseReason::Disconnected);
                    }
                }
                Vec::new()
            }
            Message::LeaseAcquire { session, takeover } => {
                let Some(terminal) = self.terminals.get_mut(&session).filter(|t| t.followed) else {
                    return refuse(ErrorCode::NotFound, "subscribe to the screen first");
                };
                if self.access != Access::Control {
                    return refuse(ErrorCode::Unauthorised, "view only");
                }
                let mut out = Vec::new();
                let reason = match &terminal.lease {
                    Some(lease) if lease.browser => {
                        return vec![host(Message::LeaseState(terminal.view(&session))).reply_to(id)];
                    }
                    Some(lease) if !takeover => {
                        return refuse(ErrorCode::LeaseHeld, &format!("{} is typing in this terminal", lease.holder));
                    }
                    Some(_) => LeaseReason::TakenOver,
                    None => LeaseReason::Granted,
                };
                self.tokens += 1;
                terminal.lease =
                    Some(FakeLease { holder: device, browser: true, token: self.tokens, next: 1, sized: false });
                terminal.reason = Some(reason);
                out.push(host(Message::LeaseState(terminal.view(&session))).reply_to(id));
                out
            }
            Message::LeaseRenew { session, lease } => {
                let Some(terminal) = self.terminals.get_mut(&session) else {
                    return refuse(ErrorCode::NotFound, "no terminal");
                };
                if !terminal.lease.as_ref().is_some_and(|l| l.browser && l.token == lease.0) {
                    return refuse(ErrorCode::Fenced, "that lease has ended");
                }
                vec![host(Message::LeaseState(terminal.view(&session))).reply_to(id)]
            }
            Message::LeaseRelease { session, lease } => {
                let Some(terminal) = self.terminals.get_mut(&session) else {
                    return refuse(ErrorCode::NotFound, "no terminal");
                };
                if !terminal.lease.as_ref().is_some_and(|l| l.browser && l.token == lease.0) {
                    return refuse(ErrorCode::Fenced, "that lease has ended");
                }
                terminal.end(&session, LeaseReason::Released).into_iter().map(host).collect()
            }
            Message::TerminalInput { session, lease, seq, data } => {
                let Some(terminal) = self.terminals.get_mut(&session) else {
                    return refuse(ErrorCode::NotFound, "no terminal");
                };
                let Some(held) = terminal.lease.as_mut().filter(|l| l.browser && l.token == lease.0) else {
                    return refuse(ErrorCode::Fenced, "that lease has ended");
                };
                if seq.0 != held.next {
                    return refuse(ErrorCode::OutOfOrder, "input out of order");
                }
                if terminal.stalled {
                    return refuse(ErrorCode::SlowClient, "the program is not reading");
                }
                held.next += 1;
                terminal
                    .change(&session, |t| {
                        t.typed.push_str(&data);
                        t.write(&data);
                    })
                    .into_iter()
                    .map(host)
                    .collect()
            }
            Message::TerminalResize { session, lease, size } => {
                let Some(terminal) = self.terminals.get_mut(&session) else {
                    return refuse(ErrorCode::NotFound, "no terminal");
                };
                let Some(held) = terminal.lease.as_mut().filter(|l| l.browser && l.token == lease.0) else {
                    return refuse(ErrorCode::Fenced, "that lease has ended");
                };
                held.sized = size.is_some();
                let to = size.map_or(terminal.size, |GridSize { cols, rows, .. }| (cols, rows));
                let mut out = terminal.set_size(&session, to);
                out.push(Message::LeaseState(terminal.view(&session)));
                out.into_iter().map(host).collect()
            }
            Message::TerminalFit { session, size } => {
                if !self.headless {
                    return refuse(ErrorCode::Unauthorised, "the fit capability was not negotiated");
                }
                let Some(terminal) = self.terminals.get_mut(&session).filter(|t| t.followed) else {
                    return refuse(ErrorCode::NotFound, "subscribe to the screen first");
                };
                terminal.size = (size.cols, size.rows);
                if terminal.lease.as_ref().is_some_and(|l| l.sized) {
                    return Vec::new();
                }
                terminal.set_size(&session, terminal.size).into_iter().map(host).collect()
            }
            Message::HistoryGet(get) => {
                let Some(terminal) = self.terminals.get(&get.session) else {
                    return refuse(ErrorCode::NotFound, "no terminal");
                };
                if get.generation.0 != terminal.generation {
                    return refuse(ErrorCode::StaleRevision, "another generation");
                }
                let first_line = terminal.screen.first_line;
                let oldest = first_line - terminal.history.len() as u64;
                let before = get.before.0.min(first_line);
                let from = before.saturating_sub(u64::from(get.count)).max(oldest);
                let mut encoder = RowsEncoder::default();
                for (row, id) in (from..before).enumerate() {
                    encoder.push(row as u16, &terminal.history[(id - oldest) as usize]);
                }
                let (styles, lines) = encoder.finish();
                let chunk = HistoryChunk {
                    session: get.session.clone(),
                    generation: Seq(terminal.generation),
                    cols: terminal.screen.cols,
                    first: Seq(from),
                    oldest: Seq(oldest),
                    evicted: false,
                    styles,
                    lines,
                };
                vec![host(Message::HistoryChunk(chunk)).reply_to(id)]
            }
            _ => Vec::new(),
        }
    }

    fn folder_page(&self, worktree: &str, path: &str, offset: u32) -> Result<atomics_core::folder::Page, WireError> {
        let paths = self
            .listings
            .get(worktree)
            .ok_or_else(|| WireError::new(ErrorCode::NotFound, "file listing is not ready"))?;
        let revision = self.board.diffs.iter().find(|d| d.worktree == worktree).map_or(1, |d| d.revision);
        atomics_core::folder::page(
            paths.iter().map(String::as_str),
            worktree,
            path,
            revision,
            offset,
            self.page_budget.saturating_sub(2048),
        )
    }
}

pub const WORKTREE: &str = "/w/repo-t1";

/// A plan with a running task and a planned one after it, its agent waiting on an approval, the chief,
/// and the task's worktree diff of two files.
pub fn board() -> Board {
    use atomics_core::diff::FileStatus;
    use atomics_core::plan::{PlanStatus, TaskStatus};
    use atomics_core::projection::{DiffFileSummary, TaskLink};
    use atomics_core::session::AgentStatus;
    let session = |id: &str, title: &str, chief: bool| SessionInfo {
        id: id.into(),
        agent: "claude".into(),
        title: title.into(),
        cwd: "/w/repo".into(),
        worktree: (!chief).then(|| WORKTREE.into()),
        branch: (!chief).then(|| "atomics/t1".into()),
        status: if chief { AgentStatus::Idle } else { AgentStatus::NeedsInput },
        chief,
        task: (!chief).then(|| TaskLink { plan: "p1".into(), task: "t1".into() }),
        pending_approvals: u32::from(!chief),
    };
    let task = |id: &str, status, depends_on: &[&str]| TaskInfo {
        id: id.into(),
        title: format!("Task {id}"),
        status,
        agent: "claude".into(),
        depends_on: depends_on.iter().map(|d| d.to_string()).collect(),
        session: (id == "t1").then(|| "s1".into()),
        branch: (id == "t1").then(|| "atomics/t1".into()),
        integrated: None,
        additions: 3,
        deletions: 1,
        report: None,
        ticket: (id == "t1").then(|| "ENG-41".into()),
    };
    let file = |path: &str| DiffFileSummary {
        old_path: Some(path.into()),
        new_path: Some(path.into()),
        status: FileStatus::Modified,
        binary: false,
        additions: 6,
        deletions: 2,
        omitted: 0,
    };
    Board {
        sessions: vec![session("chief", "Chief", true), session("s1", "Fix the parser", false)],
        plans: vec![PlanInfo {
            ticket_tree: None,
            id: "p1".into(),
            status: PlanStatus::Open,
            goal: "Parse faster".into(),
            project: Some("/w/repo".into()),
            approved: false,
            integration: None,
            tasks: vec![task("t1", TaskStatus::Running, &[]), task("t2", TaskStatus::Planned, &["t1"])],
            digest: "digest-1".into(),
            ticket: Some("ENG-40".into()),
        }],
        approvals: vec![ApprovalInfo {
            id: "a1".into(),
            session: "s1".into(),
            tool: "Bash".into(),
            command: Some("cargo test".into()),
            cwd: WORKTREE.into(),
            reason: Some("Run the tests".into()),
            warning: None,
        }],
        diffs: vec![DiffSummary {
            worktree: WORKTREE.into(),
            revision: 4,
            base_ref: "main".into(),
            base: "abc123".into(),
            partial: false,
            files: vec![file("src/a.rs"), file("src/b.rs")],
            comments: Vec::new(),
        }],
        host: HostInfo { resources: Some(resources()), ..HostInfo::default() },
    }
}

/// A file's diff: one hunk of `lines` added lines after a context line.
pub fn file_diff(path: &str, lines: u32) -> FileDiff {
    use atomics_core::diff::{DiffLine, FileStatus, Hunk, LineKind};
    let line = |kind, old_no, new_no, text: String| DiffLine { kind, old_no, new_no, text, emphasis: Vec::new() };
    let mut body = vec![line(LineKind::Context, Some(1), Some(1), "fn main() {".into())];
    body.extend(
        (0..lines).map(|n| line(LineKind::Added, None, Some(n + 2), format!("    step({n}); // {}", "x".repeat(40)))),
    );
    FileDiff {
        old_path: Some(path.into()),
        new_path: Some(path.into()),
        status: FileStatus::Modified,
        binary: false,
        hunks: vec![Hunk { header: "@@ -1,1 +1,9 @@".into(), old_start: 1, new_start: 1, lines: body }],
        omitted: 0,
    }
}

/// A finished plan with no tasks.
pub fn closed_plan(id: &str, status: atomics_core::plan::PlanStatus, goal: &str) -> PlanInfo {
    PlanInfo {
        ticket_tree: None,
        id: id.into(),
        status,
        goal: goal.into(),
        project: Some("/w/repo".into()),
        approved: true,
        integration: None,
        tasks: Vec::new(),
        digest: String::new(),
        ticket: None,
    }
}

/// [`host`] with its plan approved, its agent waiting on nothing, and a task in every state a board's buttons are
/// for: `t3` ready to start, `t4` to review on a branch, `t5` accepted and not integrated and `t6` blocked.
pub fn working() -> FakeHost {
    use atomics_core::plan::TaskStatus;
    use atomics_core::session::AgentStatus;
    let mut host = self::host();
    let branched = |id: &str, status| TaskInfo {
        id: id.into(),
        title: format!("Task {id}"),
        status,
        agent: "claude".into(),
        depends_on: Vec::new(),
        session: None,
        branch: Some(format!("atomics/{id}")),
        integrated: None,
        additions: 0,
        deletions: 0,
        report: None,
        ticket: None,
    };
    let plan = &mut host.board.plans[0];
    plan.approved = true;
    plan.tasks.extend([
        TaskInfo { branch: None, ..branched("t3", TaskStatus::Planned) },
        branched("t4", TaskStatus::Review),
        branched("t5", TaskStatus::Done),
        branched("t6", TaskStatus::Blocked),
    ]);
    host.board.approvals.clear();
    for session in &mut host.board.sessions {
        session.status = AgentStatus::Idle;
        session.pending_approvals = 0;
    }
    host
}

/// [`host`] with 26 plans, as a host that has run for a while has them: 25 archived, and in id order the open
/// plan at position 23.
pub fn many_plans() -> FakeHost {
    use atomics_core::plan::PlanStatus;
    let mut host = self::host();
    for n in 0..25 {
        let id = if n < 23 { format!("a-{n:02}") } else { format!("z-{n:02}") };
        host.board.plans.push(closed_plan(&id, PlanStatus::Archived, &format!("Archived plan number {n}")));
    }
    host
}

/// [`host`] with the workspace `team` open of the two saved, whose ticket system, `tracker` on
/// tickets.example.com, signs in from ATOMICS and stands at `state`; a sign-in under way waits on [`SIGN_IN_URL`].
pub fn tickets(state: SignInState) -> FakeHost {
    use atomics_core::projection::SignInInfo;
    let mut host = self::host();
    let url = (state == SignInState::SigningIn).then(|| SIGN_IN_URL.to_string());
    host.board.host = HostInfo {
        projects: Vec::new(),
        resources: Some(resources()),
        workspace: Some("team".into()),
        workspaces: vec!["daily".into(), "team".into()],
        sign_in: Some(SignInInfo {
            system: "tracker".into(),
            server: "tickets.example.com".into(),
            state,
            reason: None,
            url,
            rejected: None,
            browser_failed: false,
        }),
        ..HostInfo::default()
    };
    host
}

/// Two projects, a worktree and both changed and unchanged files for explorer checks.
pub fn explorer() -> FakeHost {
    use atomics_core::projection::{ProjectInfo, WorktreeInfo};
    let mut host = working();
    host.board.host.projects = vec![
        ProjectInfo {
            root: "/w/repo".into(),
            worktrees: vec![WorktreeInfo { path: WORKTREE.into(), branch: Some("atomics/t1".into()) }],
        },
        ProjectInfo { root: "/w/other".into(), worktrees: Vec::new() },
    ];
    for root in ["/w/repo", WORKTREE] {
        host.listings.insert(
            root.into(),
            vec!["README.md".into(), "src/a.rs".into(), "src/b.rs".into(), "src/unchanged.rs".into()],
        );
        host.contents.insert((root.into(), "README.md".into()), "A project readme\n".into());
        host.contents.insert((root.into(), "src/unchanged.rs".into()), "pub fn unchanged() {}\n".into());
    }
    host
}

/// The board's host with its two files' diffs, and a terminal for the agent and for the chief.
pub fn host() -> FakeHost {
    let mut host = FakeHost::new(board());
    for (path, lines) in [("src/a.rs", 6), ("src/b.rs", 40)] {
        host.files.insert((WORKTREE.into(), path.into()), file_diff(path, lines));
    }
    host.terminals.insert("s1".into(), FakeTerminal::new(80, 12, "$ "));
    host.terminals.insert("chief".into(), FakeTerminal::new(80, 12, "chief> "));
    host
}

/// A host with shared settings, pairing and a host folder picker.
pub fn management() -> FakeHost {
    use atomics_core::projection::{DeviceInfo, ManagementInfo, PairingInfo, RemoteInfo, ThemeChoice, ThemesInfo};
    let mut host = self::host();
    let choice = |name: &str, selected, file| ThemeChoice {
        name: name.into(),
        selected,
        file,
        swatches: ["#101010", "#202020", "#f0f0f0", "#c06030", "#d04040", "#40a060", "#4060d0"]
            .map(str::to_string)
            .into(),
    };
    host.board.host.management = Some(ManagementInfo {
        config: serde_json::json!({
            "values": {
                "layout.min_tile_width": {"value":520.0,"source":"default"},
                "keys.quick_open": {"value":"cmd+p","source":"default"},
                "keys.toggle_diff": {"value":"cmd+d","source":"default"},
                "agents": {"value":[{"name":"claude","command":["claude"],"kind":"claude"}],"source":"default"},
                "isolation.mode": {"value":"sandbox","source":"default"},
                "isolation.sandbox.azure.enabled": {"value":false,"source":"default"},
                "voice.api_key": {"value":"***","source":"main"},
            },
            "files": [{"source":"main","path":"~/.config/atomics/config.toml","exists":true}],
        }),
        tools: serde_json::json!({
            "hooks": false,
            "sandbox": ["✓ sbx is signed in", "✗ KVM is not available"],
            "sandbox_busy": false,
            "connections": {"rows": [{
                "name": "linear", "status": "connected", "detail": "https://mcp.linear.app/mcp · remote",
                "env": 0, "skipped": 0, "busy": false,
            }]},
            "credentials": {
                "entries": [
                    {"label": "Claude subscription", "present": true, "route": "OS keychain → ATOMICS → api.example.com",
                     "kind": "claude", "last_used": null, "sessions": []},
                    {"label": "OpenAI", "present": false, "route": "OS keychain → ATOMICS → api.example.com", "kind": "openai",
                     "last_used": null, "sessions": []},
                ],
                "invalid": 1,
                "azure": "az found: false; logged in: false; sessions: none",
            },
            "credential_busy": false,
            "credential_kinds": [
                {"id": "claude", "label": "Claude subscription"},
                {"id": "openai", "label": "OpenAI"},
                {"id": "registry", "label": "Private registry"},
            ],
        }),
        themes: ThemesInfo {
            choices: vec![choice("claude", true, false), choice("midnight", false, false), choice("dusk", false, true)],
            host_file: "config.d/host.toml".into(),
            ..Default::default()
        },
        folder_start: "/home/user".into(),
        remote: RemoteInfo {
            enabled: true,
            status: "Online".into(),
            pairing: Some(PairingInfo::Idle { note: None }),
            devices: vec![DeviceInfo {
                id: "phone".into(),
                name: "A phone".into(),
                controls: true,
                connected: false,
                seen_ms: 0,
            }],
            ..Default::default()
        },
        ..Default::default()
    });
    host
}

/// The three host-backed panels, for browser and native-client parity checks.
pub fn panels_host() -> FakeHost {
    use atomics_core::projection::{EditorInfo, PullRequestInfo};
    let mut host = host();
    host.trace.session = Some("chief".into());
    host.trace.entries.push(atomics_core::trace::TraceEntry {
        seq: 1,
        at: 0,
        phase: atomics_core::trace::TracePhase::Post,
        tool_use_id: None,
        server: "example".into(),
        tool: "lookup".into(),
        op: atomics_core::trace::TraceOp::Other,
        direct: false,
        plan_id: None,
        summary: "{}".into(),
        state: atomics_core::trace::TraceState::Ok,
        elapsed_ms: Some(10),
        cards: vec![],
        plan: None,
        result_bytes: 0,
        error: None,
        hint: None,
        truncated: false,
        omitted: 0,
    });
    let terminal = atomics_core::projection::editor_terminal(WORKTREE);
    host.terminals.insert(terminal.clone(), FakeTerminal::new(80, 12, "Helix editor\r\n"));
    host.board.host.editors.push(EditorInfo {
        root: WORKTREE.into(),
        terminal,
        running: true,
        file: Some("src/a.rs".into()),
        inlay_hints: true,
    });
    for (key, repo) in [
        ("pull:example/repo#7", "example/repo"),
        ("pull:dev.azure.com/org/project/repo#8", "dev.azure.com/org/project/repo"),
    ] {
        let file = file_diff("src/pull.rs", 6);
        let diff = atomics_core::diff::WorktreeDiff {
            base_ref: key.into(),
            base: "base-sha".into(),
            partial: false,
            files: vec![file.clone()],
        };
        host.board.diffs.push(DiffSummary::new(key, 1, &diff));
        host.files.insert((key.into(), "src/pull.rs".into()), file);
        host.board.host.pull_requests.push(PullRequestInfo {
            key: key.into(),
            repo: repo.into(),
            number: if repo.starts_with("dev.azure") { 8 } else { 7 },
            title: "Review the parser".into(),
            head: "feature".into(),
            base: "main".into(),
            url: "https://example.com/pull/7".into(),
            head_sha: Some("reviewed-sha".into()),
            loading: false,
            error: None,
        });
    }
    host
}

/// Trace fixture using the production reducer, including a completed and a live call.
pub fn trace_data() -> atomics_core::trace_feed::TraceData {
    use atomics_core::{hook::HookEvent, tickets::Cleaner, trace::TraceLog};
    let mut log = TraceLog::default();
    for (name, id, query) in [("PostToolUse", "1", "parse faster"), ("PreToolUse", "2", "follow up")] {
        let event = HookEvent::from_payload(&serde_json::json!({
            "session_id": "chief", "hook_event_name": name, "tool_name": "mcp__atomics__atomics_ticket_search",
            "tool_use_id": id, "tool_input": {"query": query, "api_key": "hidden-secret"},
            "tool_response": {"structuredContent": {"cards": [{
                "system":"example", "id":"1", "key":"ENG-1", "title":"Example ticket", "body":"Ticket details"
            }]}}
        }))
        .unwrap();
        log.ingest(&event, None, &Cleaner::default(), 0).unwrap();
    }
    atomics_core::trace_feed::TraceData {
        session: Some("chief".into()),
        entries: log.entries.into_iter().collect(),
        ..Default::default()
    }
}

/// Account usage and governor values for parity tests, with a fixed host clock.
pub fn resources() -> atomics_core::projection::ResourceInfo {
    use atomics_core::projection::{ResourceInfo, SlotInfo};
    use atomics_core::usage::{Usage, Window};
    let now = 1_800_000_000;
    let account = |session, weekly| Usage {
        session: Some(Window { remaining: session, resets_at: now + 8040 }),
        weekly: Some(Window { remaining: weekly, resets_at: now + 86400 }),
        observed_at: now - 120,
        plan: None,
    };
    ResourceInfo {
        claude: account(76.0, 24.0),
        codex: account(9.0, 100.0),
        sampled_at: now,
        load: Some(4.2),
        cores: 8,
        compiler_jobs: Some(6),
        test: SlotInfo { slots: 2, running: 1, waiting: 3 },
        qa: SlotInfo { slots: 1, running: 0, waiting: 1 },
    }
}

pub fn ticket_card(key: &str) -> atomics_core::tickets::TicketCard {
    use atomics_core::tickets::{Comment, TicketCard, TicketLink};
    TicketCard {
        link: TicketLink {
            system: "tracker".into(),
            id: key.into(),
            key: Some(key.into()),
            title: Some(format!("Fix ticket {key}")),
            state: Some("Todo".into()),
            url: Some(format!("https://tickets.example.com/{key}")),
            updated_at: Some("2026-11-04T08:50:34Z".into()),
            ..TicketLink::default()
        },
        body: Some(
            "# Description\nRead this ticket inside the app.\n\n[Documentation](https://example.com/docs)".into(),
        ),
        links: vec![atomics_core::tickets::DevLink {
            label: Some("Fix the parser".into()),
            url: Some("https://example.com/pull/7".into()),
            status: Some("Merged".into()),
            ci: Some("Succeeded".into()),
        }],
        comments: vec![Comment {
            author: Some("Example author".into()),
            text: "The reproduction is attached.".into(),
            ..Comment::default()
        }],
        snippet: Some("A searchable ticket".into()),
        score: Some(0.8),
        ..TicketCard::default()
    }
}

pub fn ticket_reader() -> FakeHost {
    let mut host = self::host();
    host.board.host.tickets = Some(atomics_core::ticket_ui::TicketsInfo {
        system: "tracker".into(),
        search: true,
        semantic_search: true,
        views: vec!["Recent".into()],
        filters: vec!["Mine".into()],
        ..Default::default()
    });
    host
}
