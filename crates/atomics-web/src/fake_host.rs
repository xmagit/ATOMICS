//! A host for the tests: the Noise responder of `atomics_core::noise` and a small `atomics.v1` server over
//! one browser channel at a time, holding a board, its event log (for replay) and file diffs. It speaks
//! the protocol the real host does, as far as the web client uses it. It uses atomics-core alone, so the
//! headless-browser test (`tests/browser.rs`) serves the real bundle from it too.
//!
//! Its terminals ([`FakeTerminal`]) echo what is typed into them, as `cat` in a cooked terminal would,
//! under leases with fencing tokens and input numbers as the real host keeps them.

use std::collections::{BTreeMap, HashMap};

use atomics_core::diff::FileDiff;
use atomics_core::lease::{GridSize, LeaseReason, LeaseState};
use atomics_core::noise::{Access, Channel, Kind, PairAnswer, PairRequest, Pairing, PublicKey, Responder, Secret};
use atomics_core::ops::{CommandResult, Outcome, PlanAction};
use atomics_core::projection::{ApprovalInfo, DiffFileBody, DiffSummary, PlanInfo, SessionInfo};
use atomics_core::screen::{HistoryChunk, Line, RowsEncoder, Screen};
use atomics_core::tunnel::close;
use atomics_core::wire::{
    self, CAPABILITIES, Disposition, Envelope, ErrorCode, Limits, Message, Resume, Revisions, Seq, Topic, VERSION,
    WireError,
};

pub const HOST_ID: &str = "6f1c2a7e-0b7d-4c55-9f43-3d2a1b0c9e8f";

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
    /// Bytes of one `diff_file` page.
    pub page_budget: usize,
    /// Every message the browser sent on an open channel.
    pub received: Vec<Message>,
    /// Terminals by session.
    pub terminals: BTreeMap<String, FakeTerminal>,
    /// The name the browser's leases are shown under: the name it paired with.
    pub device: String,
    tokens: u64,
}

/// A terminal that echoes what it is typed, with its lease.
pub struct FakeTerminal {
    pub screen: Screen,
    pub generation: u64,
    pub revision: u64,
    /// The desktop's own size, which the terminal returns to when a holder's size ends.
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
            page_budget: wire::MAX_FRAME_BYTES,
            received: Vec::new(),
            terminals: BTreeMap::new(),
            device: "Safari on iPhone".into(),
            tokens: 0,
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
            _ => {}
        }
        let envelope = Envelope::host(&self.epoch, message);
        self.events.push(envelope.clone());
        match &mut self.stage {
            Some(Stage::Open(channel)) => Self::seal(channel, &[envelope]),
            _ => Vec::new(),
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
                revisions: Revisions { sessions: rev, plans: rev, approvals: rev, diffs: rev },
                sessions: self.board.sessions.clone(),
                plans: self.board.plans.clone(),
                approvals: self.board.approvals.clone(),
                diffs: self.board.diffs.clone(),
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
                let capabilities = wire::common_capabilities(&capabilities, CAPABILITIES);
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
                    Resume::Replay { after } => out.extend(self.events.iter().skip(after.0 as usize).cloned()),
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
            | Message::HistoryGet(_)) => self.terminal_frame(message, id),
            Message::Ping { nonce } => vec![host(Message::Pong { nonce }).reply_to(id)],
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
                let plan = self.board.plans.iter().find(|p| Some(&p.id) == command.plan.as_ref()).cloned();
                let outcome = match (plan, command.action) {
                    (Some(mut plan), PlanAction::Approve)
                        if command.digest.as_deref() == Some(plan.digest.as_str()) =>
                    {
                        plan.approved = true;
                        let _ = self.event(|cursor| Message::PlanUpsert { cursor, plan });
                        Outcome::done(serde_json::Value::Null)
                    }
                    (Some(_), PlanAction::Approve) => {
                        Outcome::refused(WireError::new(ErrorCode::StaleRevision, "the plan changed"))
                    }
                    _ => Outcome::refused(WireError::new(ErrorCode::Rejected, "not in this fake")),
                };
                let mut out = Vec::new();
                if matches!(outcome, Outcome::Done { .. }) {
                    out.push(self.events.last().cloned().unwrap());
                }
                out.push(host(Message::CommandResult(CommandResult { op: command.op, outcome })));
                out
            }
            _ => Vec::new(),
        }
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
                let mut out = Vec::new();
                for topic in topics {
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
}

pub const WORKTREE: &str = "/w/repo-t1";

/// A plan with a running task and a planned one after it, its agent waiting on an approval, the chief,
/// and the task's worktree diff of two files.
pub fn board() -> Board {
    use atomics_core::diff::FileStatus;
    use atomics_core::plan::{PlanStatus, TaskStatus};
    use atomics_core::projection::{DiffFileSummary, TaskInfo, TaskLink};
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
