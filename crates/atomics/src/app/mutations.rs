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
use crate::protocol::{OpReply, OpRequest, PeerDevice};
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

/// Only the local window renders this request. One outstanding confirmation bounds both
/// retained command data and the number of prompts a controller can put on the desktop.
pub(super) struct HostConfirmation {
    pub id: String,
    pub digest: String,
    pub device: PeerDevice,
    pub details: String,
    command: HostCommand,
    key: journal::Key,
    expires: Instant,
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

/// What the chief is told with a message from a client in simple mode.
pub const SIMPLE_CONTEXT: &str = "The user is in ATOMICS's simple mode and may be new to coding agents. Reply briefly in \
     plain words, without plan or task ids, config details, or quota and agent-choice reasoning (those belong in the \
     task briefs). To propose work, make a plan: the chat shows it as a card with an Approve button, and approving \
     starts the agents at once, so say in a sentence or two what will happen and do not ask them to approve it on a \
     board or to tell you to start. Do not ask them to run terminal commands.";

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
        let first = self.mutate(key, device.as_ref(), command, Some(reply.clone()));
        self.op_replies.push((reply, op, first));
    }

    /// The desktop's own mutation, through the same journal and checks as a client's. Returns
    /// the first outcome; a later one only lands in the journal. Fresh desktop IDs
    /// bypass capacity, and journal failures never block local decisions.
    pub(super) fn mutate_desktop(&mut self, command: impl FnOnce(String) -> Command) -> Outcome {
        let op = uuid::Uuid::new_v4().to_string();
        self.mutate(journal::Key::desktop(op.clone()), None, command(op), None)
    }

    /// Carry out `command` once under `key` on behalf of the authenticated device or the desktop.
    /// Returns its outcome, or `accepted` for work that ends later, whose outcome then goes to
    /// `reply`.
    pub(super) fn mutate(
        &mut self,
        key: journal::Key,
        device: Option<&PeerDevice>,
        command: Command,
        reply: Option<OpReply>,
    ) -> Outcome {
        let desktop = key.origin == "desktop";
        match self.ops.begin(
            &key,
            device.map(|d| d.name.as_str()),
            &journal::fingerprint(&command),
            atomics_core::store::now(),
        ) {
            Begun::Fresh => {}
            Begun::Known(outcome) => return outcome,
            Begun::Running => {
                if let Some(reply) = reply {
                    if let Some(pending) = self.host_confirmation.as_mut().filter(|p| p.key == key) {
                        pending.replies.push(reply);
                    } else if let Some(flight) = self.in_flight.iter_mut().find(|f| f.key == key) {
                        flight.replies.push(reply);
                    }
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
        if let Command::SessionCommand(SessionCommand { action: SessionAction::Steer { simple, .. }, .. }) = &command {
            let who = device
                .map(|device| device.name.clone())
                .unwrap_or_else(|| if desktop { "this window".into() } else { key.origin.clone() });
            let mode = if *simple { atomics_core::config::UiMode::Simple } else { atomics_core::config::UiMode::Full };
            self.client_modes.insert(who, (mode, atomics_core::store::now()));
        }
        if !desktop && let Command::HostCommand(host) = command {
            let outcome = self.request_host_confirmation(&key, device, host, reply);
            if outcome != Outcome::Accepted {
                self.ops.finish_deferred(&key, &outcome);
            }
            return outcome;
        }
        let origin = if desktop {
            ControlOrigin::Local
        } else if let Some(device) = device.filter(|d| d.host) {
            ControlOrigin::HostDevice(device.id.clone())
        } else {
            ControlOrigin::Controller
        };
        match self.execute(command, origin) {
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
        let mut changed = self.expire_host_confirmation();
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
            self.finish_op(flight.key, flight.replies, outcome);
        }
        changed
    }

    fn finish_op(&mut self, key: journal::Key, replies: Vec<OpReply>, outcome: Outcome) {
        self.ops.finish_deferred(&key, &outcome);
        for reply in replies {
            self.op_replies.push((reply, key.op.clone(), outcome.clone()));
        }
    }

    fn device_has_host(&self, id: &str) -> bool {
        if id.starts_with("paired-") {
            self.remote.clients.as_ref().is_some_and(|clients| clients.has_host(id))
        } else {
            self.remote.devices.as_ref().is_some_and(|devices| devices.has_host(id))
        }
    }

    pub(super) fn check_input_access(&self, access: InputAccess, origin: &ControlOrigin) -> Result<(), WireError> {
        if *origin == ControlOrigin::Local {
            return Ok(());
        }
        access.check(
            matches!(origin, ControlOrigin::HostDevice(id) if access == InputAccess::Host && self.device_has_host(id)),
        )
    }

    fn request_host_confirmation(
        &mut self,
        key: &journal::Key,
        device: Option<&PeerDevice>,
        command: HostCommand,
        reply: Option<OpReply>,
    ) -> Outcome {
        if !self.window.is_attached()
            && let HostAction::SetChiefModel { agent, model, expected_session, confirm_interrupt } = &command.action
        {
            let origin = device
                .filter(|device| device.host && self.device_has_host(&device.id))
                .map_or(ControlOrigin::Controller, |device| ControlOrigin::HostDevice(device.id.clone()));
            return self
                .set_chief_model_as(*agent, model, expected_session.as_deref(), *confirm_interrupt, origin)
                .map_or_else(
                    |message| refused(ErrorCode::Rejected, message),
                    |id| Outcome::done(json!({"session": id})),
                );
        }
        let Some(device) = device.filter(|d| d.host && self.device_has_host(&d.id)) else {
            return refused(
                ErrorCode::Unauthorised,
                "Grant this paired controller Host access on the host desktop first.",
            );
        };
        if !self.window.is_attached()
            && let HostAction::OpenChiefChat { chat, .. } = &command.action
        {
            return self.open_thread(chat, &ControlOrigin::HostDevice(device.id.clone())).map_or_else(
                |message| refused(ErrorCode::Rejected, message),
                |id| Outcome::done(json!({"session":id})),
            );
        }
        if matches!(command.action, HostAction::StartChief)
            && self.bare_input_access(&self.chief_agent(self.preferred_chief().agent)) == InputAccess::Host
        {
            return self
                .ensure_chief(ControlOrigin::HostDevice(device.id.clone()))
                .map_or_else(Outcome::refused, Outcome::done);
        }
        if !self.window.is_attached() || matches!(command.action, HostAction::GrantDeviceHost { .. }) {
            return refused(ErrorCode::Unauthorised, "This action requires the host desktop.");
        }
        self.expire_host_confirmation();
        if self.host_confirmation.is_some() {
            return refused(ErrorCode::TooMany, "Another Host request is waiting on the desktop.");
        }
        let id = uuid::Uuid::new_v4().to_string();
        let request = ApprovalRequest {
            tool: "Remote Host command".into(),
            command: None,
            cwd: PathBuf::new(),
            reason: Some(device.name.clone()),
            input: json!({"device": device.id, "command": command}),
            warning: None,
        };
        let mut details = request.input["command"].clone();
        if let Some(value) = details.pointer_mut("/tool/value") {
            *value = json!("[hidden]");
        }
        if matches!(command.action, HostAction::SignInPaste { .. }) {
            details["text"] = json!("[hidden sign-in response]");
        }
        self.host_confirmation = Some(HostConfirmation {
            digest: request.digest(&id),
            id,
            details: serde_json::to_string_pretty(&details).expect("command is JSON"),
            device: device.clone(),
            command,
            key: key.clone(),
            expires: Instant::now() + Duration::from_secs(120),
            replies: reply.into_iter().collect(),
        });
        self.window.request_repaint();
        Outcome::Accepted
    }

    fn expire_host_confirmation(&mut self) -> bool {
        if let Some(pending) = self.host_confirmation.take_if(|p| Instant::now() >= p.expires) {
            self.finish_op(pending.key, pending.replies, refused(ErrorCode::Rejected, "Desktop confirmation expired."));
            return true;
        }
        false
    }

    fn decide_host_command(&mut self, source: DecisionSource<'_>, digest: Option<&str>, decision: Decision) -> Outcome {
        if let Err(error) = source.require_desktop("A remote Host action requires a decision on the host desktop.") {
            return Outcome::refused(error);
        }
        let pending = self.host_confirmation.as_ref().expect("matched approval ID");
        if digest != Some(&pending.digest) {
            return refused(ErrorCode::StaleRevision, "The decision does not match this Host request.");
        }
        let pending = self.host_confirmation.take().expect("matched approval ID and digest");
        let outcome = if decision == Decision::Deny {
            refused(ErrorCode::Rejected, "The host desktop denied this request.")
        } else if Instant::now() >= pending.expires || !self.device_has_host(&pending.device.id) {
            refused(ErrorCode::Unauthorised, "This request expired or the controller lost Host access.")
        } else {
            self.host_command(pending.command).map_or_else(Outcome::refused, Outcome::done)
        };
        self.finish_op(pending.key, pending.replies, outcome.clone());
        outcome
    }

    /// Capture the durability barrier and replies under State, run it without State.
    pub(crate) fn take_op_flush(&mut self) -> impl FnOnce() + use<> {
        let writes = std::mem::take(&mut self.trace_writes);
        let sync = self.ops.take_sync();
        let replies = std::mem::take(&mut self.op_replies);
        move || {
            for write in writes {
                write();
            }
            if let Some(file) = sync
                && let Err(error) = file.sync_data()
            {
                eprintln!("atomics: the operation journal could not sync outcomes: {error}");
            }
            // A client told an operation is done can read what it saved.
            if !replies.is_empty() {
                atomics_core::store::flush_writes();
            }
            for (reply, op, outcome) in replies {
                reply.send(&op, outcome);
            }
        }
    }

    fn execute(&mut self, command: Command, origin: ControlOrigin) -> Started {
        let desktop = origin == ControlOrigin::Local;
        let request = match command {
            Command::PlanCommand(PlanCommand { plan, action: PlanAction::Coordination { edit }, .. }) => {
                let result = plan
                    .and_then(|id| self.plans.iter().position(|p| p.id == id))
                    .ok_or_else(|| not_found("No such plan"))
                    .and_then(|index| {
                        self.edit_coordination(index, &edit, "user", "")
                            .map_err(|e| WireError::new(ErrorCode::Rejected, e))
                    });
                return Started::Now(result.map_or_else(Outcome::refused, |_| Outcome::done(json!({"updated":true}))));
            }
            Command::ApprovalDecide(ApprovalDecide { approval, digest, decision, message, .. }) => {
                let source = if desktop {
                    DecisionSource::Desktop { digest: digest.as_deref() }
                } else {
                    DecisionSource::Client { digest: digest.as_deref() }
                };
                if self.host_confirmation.as_ref().is_some_and(|p| p.id == approval) {
                    return Started::Now(self.decide_host_command(source, digest.as_deref(), decision));
                }
                return Started::Now(match self.decide(&approval, decision, message.as_deref(), source) {
                    Ok(disposition) => Outcome::done(json!({ "disposition": disposition })),
                    Err(error) => Outcome::refused(error),
                });
            }
            Command::ReviewCommand(review) => {
                return Started::Now(self.review(review, &origin).map_or_else(Outcome::refused, Outcome::done));
            }
            Command::HostCommand(host) => {
                return Started::Now(self.host_command(host).map_or_else(Outcome::refused, Outcome::done));
            }
            Command::PlanCommand(PlanCommand {
                plan,
                action: PlanAction::PublishDecision { request, approve },
                ..
            }) => {
                let result = plan
                    .and_then(|id| self.plans.iter().position(|p| p.id == id))
                    .ok_or_else(|| not_found("No such plan"))
                    .and_then(|index| {
                        self.decide_publish(index, &request, approve)
                            .map_err(|e| WireError::new(ErrorCode::Malformed, e))
                    });
                return Started::Now(
                    result.map_or_else(Outcome::refused, |_| Outcome::done(json!({"accepted": true}))),
                );
            }
            Command::PlanCommand(plan) => self.plan_request(plan),
            Command::SessionCommand(session) => self.session_request(session, &origin),
        };
        match request {
            Ok(request) => self.run_control(request, origin),
            Err(error) => Started::Now(Outcome::refused(error)),
        }
    }

    /// Run what ctl runs, and see whether it answered at once.
    fn run_control(&mut self, request: ControlRequest, origin: ControlOrigin) -> Started {
        let (reply, outcome) = std::sync::mpsc::channel();
        self.control_from(request, reply, origin);
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
            // Only the protocol connection holding the private reply opens the socket.
            HostAction::OpenMachinePort { .. } => Ok(json!({ "allowed": true })),
            HostAction::Tickets { request, ticket } => {
                self.ticket_ui_command(request, *ticket).map_err(rejected)?;
                Ok(json!({ "queued": true }))
            }
            HostAction::Edit { root, path, line } => {
                let root = PathBuf::from(root);
                if !self.diffs.contains_key(&root) && !self.sessions.iter().any(|s| s.root() == root) {
                    return Err(not_found("no worktree at that root"));
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
            HostAction::StartPlan { request, text, project, fresh } => {
                let id = format!("plan-{request}");
                if self.plans.iter().any(|p| p.id == id) {
                    return Ok(json!({ "plan": id }));
                }
                let project = project
                    .as_deref()
                    .map(|path| workspace::project_root(std::path::Path::new(path)))
                    .transpose()
                    .map_err(|e| rejected(format!("Could not open this project: {e}")))?;
                let thread = if fresh { format!("chat-{request}") } else { self.store.active_chief_chat.clone() };
                let title: String = text.trim().lines().next().unwrap_or_default().trim().chars().take(120).collect();
                let plan = Plan {
                    id: id.clone(),
                    goal: title.clone(),
                    first_message: Some(title),
                    thread: thread.clone(),
                    project,
                    created_at: super::remote::now_ms(),
                    ..Plan::default()
                };
                let home = paths::atomics_home();
                plan.save_later_in(&home).map_err(|e| rejected(format!("Could not save the new plan: {e}")))?;
                let prompt = match &plan.project {
                    Some(project) => format!("{}\n\nProject: {}", text.trim(), project.display()),
                    None => text.trim().to_owned(),
                };
                if let Err(error) = self.deliver_to_thread(&thread, &format!("[Plan {id}]\n{prompt}"), false) {
                    std::fs::remove_file(home.join("plans").join(format!("{id}.json")))
                        .map_err(|e| rejected(format!("{error}. Could not discard the unsent plan: {e}")))?;
                    return Err(rejected(error));
                }
                if let Some(root) = &plan.project {
                    self.add_project(root);
                }
                self.plans.push(plan);
                if let Err(error) = self.open_thread(&thread, &ControlOrigin::Local) {
                    self.toast(error);
                }
                Ok(json!({ "plan": id }))
            }
            HostAction::StartChief => self.ensure_chief(ControlOrigin::Local),
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
        let replaces = matches!(action, PlanAction::Approve | PlanAction::ApproveAndStart)
            || (matches!(action, PlanAction::Set { .. }) && index.is_some());
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
            PlanAction::Coordination { .. } => {
                return Err(WireError::new(
                    ErrorCode::Rejected,
                    "Coordination edits require the authenticated user path",
                ));
            }
            PlanAction::PublishDecision { .. } => {
                return Err(WireError::new(ErrorCode::Malformed, "Publish decisions require the user interface"));
            }
            PlanAction::Notebook { id, pinned, delete } => {
                ControlRequest::CurateNote { plan: plan.unwrap_or_default(), id, pinned, delete }
            }
            PlanAction::Set { goal, tasks, integration } => ControlRequest::PlanSet {
                plan,
                goal,
                tasks,
                integration,
                review_agent: None,
                review_model: None,
                ticket: None,
            },
            PlanAction::Approve | PlanAction::ApproveAndStart => {
                ControlRequest::PlanApprove { plan, start: Some(matches!(action, PlanAction::ApproveAndStart)) }
            }
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
                ControlRequest::PlanIntegrate { plan, id, commit: None, override_busy: false }
            }
            PlanAction::Archive => ControlRequest::PlanClose {
                reviewed: false,
                plan: plan.unwrap_or_default(),
                status: PlanStatus::Archived,
            },
            PlanAction::Complete => ControlRequest::PlanClose {
                reviewed: false,
                plan: plan.unwrap_or_default(),
                status: PlanStatus::Completed,
            },
        })
    }

    fn session_request(
        &mut self,
        command: SessionCommand,
        origin: &ControlOrigin,
    ) -> Result<ControlRequest, WireError> {
        Ok(match command.action {
            SessionAction::Steer { session, text, now, simple } => {
                let index = self.session_by_id(&session)?;
                self.check_input_access(self.sessions[index].input_access, origin)?;
                if simple && let Some(chief) = self.sessions.iter_mut().find(|s| s.id == session && s.chief) {
                    chief.simple_context_for.clone_from(&chief.claude_id);
                }
                ControlRequest::Send { session, text, raw: false, now }
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
                    interactive: *origin == ControlOrigin::Local,
                }
            }
            SessionAction::Close { session } => {
                self.session_by_id(&session)?;
                ControlRequest::Close { session }
            }
        })
    }

    fn review(&mut self, command: ReviewCommand, origin: &ControlOrigin) -> Result<serde_json::Value, WireError> {
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
                self.deliver_review(&root, origin).map_err(|message| WireError::new(ErrorCode::Rejected, message))
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
    pub(super) fn deliver_review(&mut self, root: &Path, origin: &ControlOrigin) -> Result<serde_json::Value, String> {
        let count = self.diffs.get(root).map_or(0, |d| d.comments.len());
        Ok(match self.send_review(root, origin)? {
            Delivery::Held => json!({ "held_comments": count, "held": true }),
            Delivery::Pending => json!({ "pending_comments": count, "sent": false }),
            _ => json!({ "sent_comments": count }),
        })
    }
}
