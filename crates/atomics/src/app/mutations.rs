//! Mutations, journaled: those protocol clients send and the desktop's own approve and steer.
//!
//! Every one runs here, on the runtime thread under the `State` lock, one at a time and in turn
//! with the desktop's actions and ctl's commands. That is what gives an approval exactly one
//! winner: the first decision for its ID answers it (`State::decide`), and any later one, from
//! another client, the desktop or a retry with a new ID, finds it answered. The journal
//! (`protocol::journal`) records an operation before it takes effect and its outcome after, so a
//! retry of the same ID is answered from it instead of being carried out again.
//!
//! The commands reuse what ctl and MCP run (`State::control`), with the protocol's IDs resolved
//! exactly and its staleness checks first: a plan's approval scope (`digest`) for approving or
//! replacing it, a diff's revision for a comment. Work that ends later (a delivery to confirm, a
//! worktree to create, a merge) answers `accepted` and then its outcome, when `poll_ops` sees it.

use std::sync::mpsc::TryRecvError;

use super::*;
use crate::protocol::journal::{self, Begun};
use crate::protocol::{OpReply, OpRequest};
use atomics_core::ops::{
    ApprovalDecide, Command, HostAction, HostCommand, Outcome, PlanAction, PlanCommand, ReviewAction, ReviewCommand,
    SessionAction, SessionCommand,
};

/// A mutation whose outcome comes later, and who waits for it.
pub(super) struct InFlight {
    key: journal::Key,
    outcome: Receiver<ControlOutcome>,
    replies: Vec<OpReply>,
}

enum Started {
    Now(Outcome),
    Later(Receiver<ControlOutcome>),
}

fn refused(code: ErrorCode, message: impl Into<String>) -> Outcome {
    Outcome::refused(WireError::new(code, message))
}

fn not_found(message: impl Into<String>) -> WireError {
    WireError::new(ErrorCode::NotFound, message)
}

/// The protocol's outcome for an answer from `State::control`. A delivery that could not be
/// confirmed says so (`unconfirmed`); it is uncertain, not refused.
fn from_control(outcome: ControlOutcome) -> Outcome {
    match outcome {
        ControlOutcome::Done(response) if response.ok => Outcome::done(response.result),
        ControlOutcome::Done(response) => {
            let message = response.error.unwrap_or_else(|| "refused".into());
            if response.result.get("unconfirmed") == Some(&json!(true)) {
                Outcome::Uncertain { message }
            } else {
                refused(ErrorCode::Rejected, message)
            }
        }
        _ => refused(ErrorCode::Internal, "the command answered in an unexpected way"),
    }
}

impl State {
    /// A sender can be passed to the server before any journal file is opened.
    pub(crate) fn op_sender(&self) -> crate::host::Sender<OpRequest> {
        self.op_tx.clone()
    }

    /// Open the journal only after server startup succeeded. A failed open disables
    /// client mutations; fresh desktop actions remain available.
    pub(crate) fn serve_commands(&mut self, journal: &Path) {
        match journal::Journal::open(journal, atomics_core::store::now()) {
            Ok(opened) => self.ops = opened,
            Err(error) => {
                eprintln!("atomics: client mutations disabled: operation journal cannot be opened: {error}");
                self.ops = journal::Journal::unwritable(error.to_string());
            }
        }
    }

    /// A mutation from a protocol client: what is known now goes back at once, and the final
    /// outcome of work that ends later when it does.
    pub(super) fn serve_op(&mut self, request: OpRequest) {
        let OpRequest { client, device, command, reply } = request;
        // A paired device's operations are its own, and the journal names it.
        let key = match &device {
            Some(device) => journal::Key::device(&device.id, &client, command.op()),
            None => journal::Key::client(&client, command.op()),
        };
        let op = command.op().to_string();
        let by = device.map(|device| device.name);
        let first = self.mutate(key, by.as_deref(), command, Some(reply.clone()));
        self.op_replies.push((reply, op, first));
    }

    /// The desktop's own mutation, through the same journal and checks as a client's. Returns
    /// the first outcome; a later one only lands in the journal. Fresh desktop IDs
    /// bypass capacity, and journal failures never block local decisions.
    pub(super) fn mutate_desktop(&mut self, command: impl FnOnce(String) -> Command) -> Outcome {
        let op = uuid::Uuid::new_v4().to_string();
        self.mutate(journal::Key::desktop(op.clone()), None, command(op), None)
    }

    /// Carry out `command` once under `key` (sent by the paired device named `by`, if any).
    /// Returns its outcome, or `accepted` for work that ends later, whose outcome then goes to
    /// `reply`.
    pub(super) fn mutate(
        &mut self,
        key: journal::Key,
        by: Option<&str>,
        command: Command,
        reply: Option<OpReply>,
    ) -> Outcome {
        let desktop = key.origin == "desktop";
        match self.ops.begin(&key, by, &journal::fingerprint(&command), atomics_core::store::now()) {
            Begun::Fresh => {}
            Begun::Known(outcome) => return outcome,
            Begun::Running => {
                if let (Some(reply), Some(flight)) = (reply, self.in_flight.iter_mut().find(|f| f.key == key)) {
                    flight.replies.push(reply);
                }
                return Outcome::Accepted;
            }
            Begun::Conflict => return refused(ErrorCode::Conflict, "this operation ID was used for another command"),
            Begun::Full => return refused(ErrorCode::TooMany, "too many operations to remember; try again shortly"),
            Begun::Unwritable(_) if desktop => {}
            Begun::Unwritable(why) => {
                return refused(ErrorCode::Internal, format!("the operation journal cannot be written: {why}"));
            }
        }
        match self.execute(command) {
            Started::Now(outcome) => {
                self.ops.finish_deferred(&key, &outcome);
                outcome
            }
            Started::Later(outcome) => {
                self.in_flight.push(InFlight { key, outcome, replies: reply.into_iter().collect() });
                Outcome::Accepted
            }
        }
    }

    /// Finish the mutations whose outcomes have arrived.
    pub(super) fn poll_ops(&mut self) -> bool {
        if self.in_flight.is_empty() {
            return false;
        }
        let mut changed = false;
        for flight in std::mem::take(&mut self.in_flight) {
            let outcome = match flight.outcome.try_recv() {
                Ok(outcome) => from_control(outcome),
                Err(TryRecvError::Empty) => {
                    self.in_flight.push(flight);
                    continue;
                }
                Err(TryRecvError::Disconnected) => {
                    Outcome::Uncertain { message: "the host lost track of this operation; check the board".into() }
                }
            };
            changed = true;
            self.ops.finish_deferred(&flight.key, &outcome);
            for reply in &flight.replies {
                self.op_replies.push((reply.clone(), flight.key.op.clone(), outcome.clone()));
            }
        }
        changed
    }

    /// Capture the durability barrier and replies under State, run it without State.
    pub(crate) fn take_op_flush(&mut self) -> impl FnOnce() + use<> {
        let sync = self.ops.take_sync();
        let replies = std::mem::take(&mut self.op_replies);
        move || {
            if let Some(file) = sync
                && let Err(error) = file.sync_data()
            {
                eprintln!("atomics: the operation journal could not sync outcomes: {error}");
            }
            for (reply, op, outcome) in replies {
                reply.send(&op, outcome);
            }
        }
    }

    fn execute(&mut self, command: Command) -> Started {
        let request = match command {
            Command::ApprovalDecide(ApprovalDecide { approval, decision, message, .. }) => {
                return Started::Now(match self.decide(&approval, decision, message.as_deref()) {
                    Ok(disposition) => Outcome::done(json!({ "disposition": disposition })),
                    Err(error) => Outcome::refused(error),
                });
            }
            Command::ReviewCommand(review) => {
                return Started::Now(self.review(review).map_or_else(Outcome::refused, Outcome::done));
            }
            Command::HostCommand(host) => {
                return Started::Now(self.host_command(host).map_or_else(Outcome::refused, Outcome::done));
            }
            Command::PlanCommand(plan) => self.plan_request(plan),
            Command::SessionCommand(session) => self.session_request(session),
        };
        match request {
            Ok(request) => self.run_control(request),
            Err(error) => Started::Now(Outcome::refused(error)),
        }
    }

    /// Run what ctl runs, and see whether it answered at once.
    fn run_control(&mut self, request: ControlRequest) -> Started {
        let (reply, outcome) = std::sync::mpsc::channel();
        self.control(request, reply);
        match outcome.try_recv() {
            Ok(answer) => Started::Now(from_control(answer)),
            Err(TryRecvError::Empty) => Started::Later(outcome),
            Err(TryRecvError::Disconnected) => Started::Now(Outcome::Uncertain {
                message: "the command ended without an answer; check the board".into(),
            }),
        }
    }

    /// The ticket system's sign-in and the open workspace, for a client. A client's sign-in never opens a
    /// browser here: the client shows the address and pastes back where its browser ended. What becomes of
    /// the sign-in shows in the host's state, not in this outcome.
    pub(super) fn host_command(&mut self, command: HostCommand) -> Result<serde_json::Value, WireError> {
        let rejected = |message: String| WireError::new(ErrorCode::Rejected, message);
        match command.action {
            HostAction::Tickets { request, ticket } => {
                self.ticket_ui_command(request, *ticket).map_err(rejected)?;
                Ok(json!({ "queued": true }))
            }
            HostAction::Edit { root, path, line } => {
                let root = PathBuf::from(root);
                if !self.diffs.contains_key(&root) && !self.sessions.iter().any(|s| s.root() == root) {
                    return Err(not_found("no worktree at that root"));
                }
                if path.as_ref().is_some_and(|p| {
                    std::path::Path::new(p).is_absolute()
                        || std::path::Path::new(p).components().any(|c| matches!(c, std::path::Component::ParentDir))
                }) {
                    return Err(rejected("name a file inside the worktree".into()));
                }
                self.edit_terminal(&root, path.as_deref(), line).map_err(rejected)?;
                if !self.editors.get(&root).is_some_and(|t| !t.exited()) {
                    return Err(rejected("the editor could not be started".into()));
                }
                Ok(json!({ "terminal": atomics_core::projection::editor_terminal(&root.to_string_lossy()) }))
            }
            HostAction::EditorHints { root } => {
                let editor = self
                    .editors
                    .get(std::path::Path::new(&root))
                    .filter(|t| !t.exited())
                    .ok_or_else(|| not_found("no running editor at that root"))?;
                editor.type_steps(self.config.editor.inlay_hints_keys.clone(), std::time::Duration::ZERO);
                Ok(json!({ "typed": true }))
            }
            HostAction::PullRequestDiff { key } => {
                if !self.load_pull_request(&key) {
                    return Err(not_found("no pull request with that key"));
                }
                Ok(json!({ "loading": key }))
            }
            HostAction::StartChief => {
                if !self.sessions.iter().any(|s| s.chief && s.terminal.as_ref().is_some_and(|t| !t.exited())) {
                    self.sessions.retain(|s| !s.chief);
                    self.start_chief(None).map_err(rejected)?;
                }
                Ok(json!({ "started": true }))
            }
            HostAction::SignIn => {
                self.start_client_sign_in().map_err(rejected)?;
                Ok(json!({ "signing_in": true }))
            }
            HostAction::SignInPaste { text } => {
                self.paste_signin(&text).map_err(rejected)?;
                Ok(json!({ "pasted": true }))
            }
            HostAction::SignInCancel => {
                self.cancel_sign_in().map_err(rejected)?;
                Ok(json!({ "cancelled": true }))
            }
            HostAction::OpenWorkspace { name } => {
                if !Workspace::list().contains(&name) {
                    return Err(not_found(format!("no saved workspace {name}")));
                }
                let projects = self.open_workspace(&name).map_err(rejected)?;
                Ok(json!({ "workspace": name, "projects": projects }))
            }
            action => self.manage(action),
        }
    }

    /// The session with exactly this ID; the protocol never matches titles or prefixes.
    fn session_by_id(&self, id: &str) -> Result<usize, WireError> {
        self.sessions.iter().position(|s| s.id == id).ok_or_else(|| not_found(format!("no session {id}")))
    }

    fn plan_request(&self, command: PlanCommand) -> Result<ControlRequest, WireError> {
        let PlanCommand { plan, digest, action, .. } = command;
        let index = match &plan {
            Some(id) => {
                Some(self.plans.iter().position(|p| p.id == *id).ok_or_else(|| not_found(format!("no plan {id}")))?)
            }
            None => None,
        };
        // Approving or replacing a plan is about the plan the user saw: its approval scope.
        let replaces =
            matches!(action, PlanAction::Approve) || (matches!(action, PlanAction::Set { .. }) && index.is_some());
        if replaces && digest.is_none() {
            return Err(WireError::new(ErrorCode::Malformed, "name the digest of the plan the user reviewed"));
        }
        if let (Some(index), Some(digest)) = (index, &digest)
            && self.plans[index].digest() != *digest
        {
            return Err(WireError::new(ErrorCode::StaleRevision, "the plan changed since; review it again"));
        }
        let task = |id: &str| -> Result<(), WireError> {
            let found = index.is_some_and(|i| self.plans[i].tasks.iter().any(|t| t.id == id));
            if found { Ok(()) } else { Err(not_found(format!("no task {id}"))) }
        };
        Ok(match action {
            PlanAction::Notebook { id, pinned, delete } => {
                ControlRequest::CurateNote { plan: plan.unwrap_or_default(), id, pinned, delete }
            }
            PlanAction::Set { goal, tasks, integration } => {
                ControlRequest::PlanSet { plan, goal, tasks, integration, ticket: None }
            }
            PlanAction::Approve => ControlRequest::PlanApprove { plan },
            PlanAction::Dispatch { task: id } => {
                if let Some(id) = &id {
                    task(id)?;
                }
                ControlRequest::PlanDispatch { plan, id }
            }
            PlanAction::Update { task: id, status, note } => {
                task(&id)?;
                let status = status.map(|s| s.label().to_string());
                ControlRequest::PlanUpdate { plan, id, status, note, session: None }
            }
            PlanAction::Integrate { task: id } => {
                task(&id)?;
                ControlRequest::PlanIntegrate { plan, id }
            }
            PlanAction::Archive => {
                ControlRequest::PlanClose { plan: plan.unwrap_or_default(), status: PlanStatus::Archived }
            }
            PlanAction::Complete => {
                ControlRequest::PlanClose { plan: plan.unwrap_or_default(), status: PlanStatus::Completed }
            }
        })
    }

    fn session_request(&self, command: SessionCommand) -> Result<ControlRequest, WireError> {
        Ok(match command.action {
            SessionAction::Steer { session, text } => {
                self.session_by_id(&session)?;
                ControlRequest::Send { session, text, raw: false }
            }
            SessionAction::Start { agent, folder, repo, branch, prompt } => {
                if self.config.agent(&agent).is_none() {
                    return Err(not_found(format!("no agent {agent} is configured")));
                }
                ControlRequest::Start {
                    agent,
                    folder,
                    repo,
                    branch,
                    prompt,
                    args: Vec::new(),
                    isolation: None,
                    interactive: true,
                }
            }
            SessionAction::Close { session } => {
                self.session_by_id(&session)?;
                ControlRequest::Close { session }
            }
        })
    }

    fn review(&mut self, command: ReviewCommand) -> Result<serde_json::Value, WireError> {
        let root = PathBuf::from(&command.worktree);
        let Some(data) = self.diffs.get_mut(&root).filter(|data| data.prepared.is_some()) else {
            return Err(not_found("no diff for that worktree"));
        };
        match command.action {
            ReviewAction::Add { revision, path, line, removed, text } => {
                if revision.0 != data.revision {
                    return Err(WireError::new(
                        ErrorCode::StaleRevision,
                        format!("the diff is at revision {}; anchor the comment again", data.revision),
                    ));
                }
                let in_diff = data.prepared.as_ref().is_some_and(|p| p.diff.files.iter().any(|f| f.path() == path));
                if !in_diff {
                    return Err(not_found(format!("{path} is not in the diff")));
                }
                let pending = self.add_comment(&root, path, line, removed, text);
                Ok(json!({ "pending_comments": pending }))
            }
            ReviewAction::Remove { path, line, removed, text } => {
                let at = data
                    .comments
                    .iter()
                    .position(|c| c.path == path && c.line == line && c.removed == removed && c.body == text)
                    .ok_or_else(|| not_found("no such comment"))?;
                data.comments.remove(at);
                Ok(json!({ "pending_comments": data.comments.len() }))
            }
            ReviewAction::Send => {
                self.deliver_review(&root).map_err(|message| WireError::new(ErrorCode::Rejected, message))
            }
        }
    }

    /// Add a comment on a line of `root`'s diff (or the whole file), quoting the line. Returns how
    /// many wait to be sent. ctl's `comment` and the protocol's `review_command` share it.
    pub(super) fn add_comment(
        &mut self,
        root: &Path,
        path: String,
        line: Option<u32>,
        removed: bool,
        body: String,
    ) -> usize {
        let state = self.diffs.entry(root.to_path_buf()).or_default();
        let file = state.prepared.as_ref().and_then(|p| p.diff.files.iter().find(|f| f.path() == path));
        let excerpt = file
            .and_then(|f| {
                f.hunks.iter().flat_map(|h| &h.lines).find(|l| {
                    let number = if removed { l.old_no } else { l.new_no };
                    line.is_some() && number == line && (l.kind == atomics_core::diff::LineKind::Removed) == removed
                })
            })
            .map(|l| l.text.clone())
            // Untracked content a diff off screen left out is read from the file.
            .or_else(|| {
                let n = line.filter(|_| !removed && file.is_some_and(|f| f.omitted > 0))?;
                let text = std::fs::read_to_string(root.join(&path)).ok()?;
                text.lines().nth((n as usize).checked_sub(1)?).map(str::to_string)
            })
            .unwrap_or_default();
        state.comments.push(steer::ReviewComment { path, line, removed, excerpt, body });
        state.comments.len()
    }

    /// Send `root`'s comments to its agent as one message. ctl's `send_review` and the protocol's
    /// `review_command` share it.
    pub(super) fn deliver_review(&mut self, root: &Path) -> Result<serde_json::Value, String> {
        let count = self.diffs.get(root).map_or(0, |d| d.comments.len());
        Ok(match self.send_review(root)? {
            Delivery::Held => json!({ "held_comments": count, "held": true }),
            Delivery::Pending => json!({ "pending_comments": count, "sent": false }),
            _ => json!({ "sent_comments": count }),
        })
    }
}
