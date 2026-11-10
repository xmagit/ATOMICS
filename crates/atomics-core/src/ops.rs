//! Mutations: what a client asks the host to change, and what became of it.
//!
//! Browser-safe: serde types and pure functions only. Every mutation carries an operation ID the
//! client chose (`op`). The host journals it before any side effect and answers with
//! `command_result`, once or twice: `accepted` while work that takes a while runs, then `done`,
//! `refused` or `uncertain`. Sending the same `op` again returns the known outcome instead of
//! doing the work twice; a host that stopped while the work ran says `uncertain`, never a guess.
//! The host keeps outcomes for at least `OP_TTL_SECS` (advertised in `welcome`'s limits); a
//! client never resends an older operation, which needs the user's fresh intent ([`Pending`]).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::approvals::Decision;
use crate::plan::{Task, TaskStatus};
use crate::wire::{ErrorCode, MAX_ID_BYTES, Message, Revision, WireError};

/// The longest text a mutation carries: a steering message, a comment, a note or a goal.
pub const MAX_TEXT_BYTES: usize = 64 * 1024;
/// The most tasks a `plan_command` `set` may carry.
pub const MAX_TASKS: usize = 256;
/// How long the host remembers an operation, from when it began. A client resends an
/// unanswered operation only while it is younger than this.
pub const OP_TTL_SECS: u32 = 15 * 60;

/// What a client may change. The host derives it from the client's role and the capabilities it
/// negotiated, and lists it in `welcome` (`may`), so a client can tell what to offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grant {
    /// Take a terminal's lease and type into it (`input`).
    Type,
    /// `approval_decide`.
    Approve,
    /// `session_command`: steer, start and close agents.
    Session,
    /// `plan_command`.
    Plan,
    /// `review_command`.
    Review,
    /// `host_command`: sign-in, settings proposals, pairing, setup and host projects.
    Host,
    /// A grant from a newer host.
    #[serde(other)]
    Unknown,
}

/// Answer a pending tool approval. `approval` is the ID the host gave that one request, so a
/// decision can never answer a later request in the same session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalDecide {
    pub op: String,
    pub approval: String,
    pub decision: Decision,
    /// Told to the agent with the decision (a reason to deny, say).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A change to a plan. `digest` is the plan's `digest` as the client shows it: `approve` and a
/// `set` of an existing plan need it, and any action that carries one is refused as
/// `stale_revision` once the plan's approval scope has changed since.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanCommand {
    pub op: String,
    /// Absent only for a `set` that creates a plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(flatten)]
    pub action: PlanAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PlanAction {
    /// User curation of a notebook proposal, visible to every board client.
    Notebook {
        id: u64,
        #[serde(default)]
        pinned: Option<bool>,
        #[serde(default)]
        delete: bool,
    },
    /// Write the goal and tasks, as the chief's `plan_set` does. An approved plan stays approved
    /// only if its approval scope is unchanged.
    Set {
        goal: String,
        tasks: Vec<Task>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        integration: Option<String>,
    },
    Approve,
    /// Start the given task, or every ready one.
    Dispatch {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<String>,
    },
    /// Change a task's status, add a note to it, or both.
    Update {
        task: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<TaskStatus>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// Commit the task's work and merge its branch into the plan's integration branch.
    Integrate {
        task: String,
    },
    /// Close the plan as archived; its tasks and history stay.
    Archive,
    /// Close the plan as completed, preserving its tasks and history.
    Complete,
}

/// Review comments on a worktree's diff. They are shared: every client sees them in the diff
/// summary until they are sent to the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCommand {
    pub op: String,
    pub worktree: String,
    #[serde(flatten)]
    pub action: ReviewAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ReviewAction {
    /// Comment on a line (new file, or old file when `removed`), or on the whole file without
    /// `line`. Refused as `stale_revision` unless the diff is still at `revision`: the client
    /// refreshes the line it anchored to and sends it again.
    Add {
        revision: Revision,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        removed: bool,
        text: String,
    },
    /// Remove the comment with these fields (one of them, if several are alike).
    Remove {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        removed: bool,
        text: String,
    },
    /// Send every comment to the worktree's agent as one message.
    Send,
}

/// Agents: steer one, start one, close one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCommand {
    pub op: String,
    #[serde(flatten)]
    pub action: SessionAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SessionAction {
    /// Type a message into the agent and submit it, or hold or queue it as the desktop's steer
    /// bar does. The result says which (`sent`, `held`, `queued_until_next_hook`).
    Steer { session: String, text: String },
    /// Start a configured agent in `folder`, in a new worktree of `repo` on `branch`, or in a
    /// scratch folder of its own without either.
    Start {
        agent: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        folder: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    /// End the agent and close its tab.
    Close { session: String },
}

/// The host's own state: the ticket system's sign-in and the workspace open there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCommand {
    pub op: String,
    #[serde(flatten)]
    pub action: HostAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HostAction {
    Edit {
        root: String,
        path: Option<String>,
        line: Option<u32>,
    },
    EditorHints {
        root: String,
    },
    PullRequestDiff {
        key: String,
    },
    StartChief,
    /// Start the ticket system's sign-in on the host (or join the one under way). The host opens no
    /// browser for a client: the host's `sign_in.url` is the address to open, and the address the
    /// browser ends on comes back with `sign_in_paste`. Once it finishes, the ticket worker
    /// connects again with the new sign-in.
    SignIn,
    /// The address (or just its query) a browser ended on after signing in, for the sign-in under
    /// way. One that does not match it is refused there and the sign-in keeps waiting.
    SignInPaste {
        text: String,
    },
    /// Stop the sign-in under way.
    SignInCancel,
    /// Open a saved workspace on the host, by name, in place of the projects open there.
    OpenWorkspace {
        name: String,
    },
    /// Search, read and plan from the configured ticket system.
    Tickets {
        request: String,
        ticket: Box<crate::ticket_ui::TicketAction>,
    },
    SaveWorkspace {
        name: String,
    },
    /// All paths name folders on the host, never on the client.
    BrowseFolder {
        path: String,
    },
    BrowseFolderPage {
        path: String,
        offset: u32,
    },
    OpenProject {
        path: String,
    },
    CloseProject {
        path: String,
    },
    ConfigPropose {
        scope: crate::selfconfig::Scope,
        toml: String,
        #[serde(default)]
        remove: Vec<String>,
        #[serde(default)]
        theme: Option<String>,
        #[serde(default)]
        reason: Option<String>,
    },
    ConfigDecide {
        id: String,
        apply: bool,
        note: String,
    },
    /// Show a theme and save its name, in `config.toml` or, with `host`, in this host's own file. The user's own
    /// pick, undone by picking another, so no proposal.
    SelectTheme {
        name: String,
        #[serde(default)]
        host: bool,
    },
    /// Host setup and credential-store operations.
    SettingsTool {
        tool: SettingsTool,
    },
    PairDevice,
    CancelPairing,
    /// Bind the decision to the exact device/code the user compared.
    DecidePairing {
        code: String,
        controls: Option<bool>,
    },
    RevokeDevice {
        id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case")]
pub enum SettingsTool {
    InstallHooks,
    CheckSandbox,
    RefreshConnections,
    ImportConnections,
    Connect {
        name: String,
        store_env: bool,
    },
    Disconnect {
        name: String,
    },
    AddConnection {
        name: String,
        url: String,
    },
    RefreshCredentials,
    PutCredential {
        kind: String,
        value: String,
        host: Option<String>,
    },
    /// Turn the Azure CLI relay on or off, in `config.toml`. The user's own switch, so no proposal.
    SetAzure {
        enabled: bool,
    },
    RemoveCredential {
        kind: String,
        host: Option<String>,
    },
    CleanCredentials,
    /// Look for git and the agents again, with the login shell's PATH as it is now.
    CheckPrograms,
    /// Where agents run on this machine, in this host's own config file. The user's own switch, so no proposal.
    SetIsolation {
        mode: crate::isolation::IsolationMode,
    },
    /// The setup screen was finished or skipped, so it no longer opens by itself. Settings brings it back.
    FinishSetup,
    /// Make a git repository of a folder on the host and open it as a project.
    InitProject {
        path: String,
    },
    /// Answer the question a start that could not get a sandbox asked: run it on this machine, or leave it.
    AnswerStart {
        run_here: bool,
    },
    /// Run one setup step on the host, off its window's thread; [`crate::projection::SetupInfo`] says how it goes.
    RunSetup {
        action: crate::projection::SetupAction,
        /// What was pasted for it, over this connection only: it reaches no argument, log or reply.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
    },
    /// Stop the setup step under way.
    CancelSetup,
    /// A connection can wait, or no longer has to. It stays that way through restarts and updates.
    ConnectLater {
        id: String,
        later: bool,
    },
}

/// What became of an operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    /// Journaled and under way; another `command_result` for the same `op` ends it.
    Accepted,
    Done {
        #[serde(default, skip_serializing_if = "Value::is_null")]
        result: Value,
    },
    /// Not carried out; nothing changed.
    Refused { code: ErrorCode, message: String },
    /// The host stopped while it ran, or cannot confirm it: it may or may not have taken effect.
    /// Look at the board before asking again (with a new `op`).
    Uncertain { message: String },
    /// A status from a newer host.
    #[serde(other)]
    Unknown,
}

impl Outcome {
    pub fn done(result: Value) -> Self {
        Outcome::Done { result }
    }

    pub fn refused(error: WireError) -> Self {
        Outcome::Refused { code: error.code, message: error.message }
    }

    /// Whether no later `command_result` follows for the operation.
    pub fn is_final(&self) -> bool {
        !matches!(self, Outcome::Accepted)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandResult {
    pub op: String,
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// A mutation, whichever message carried it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Command {
    ApprovalDecide(ApprovalDecide),
    PlanCommand(PlanCommand),
    ReviewCommand(ReviewCommand),
    SessionCommand(SessionCommand),
    HostCommand(HostCommand),
}

impl Command {
    /// The mutation a message carries, if it carries one.
    pub fn from_message(message: Message) -> Option<Command> {
        match message {
            Message::ApprovalDecide(c) => Some(Command::ApprovalDecide(c)),
            Message::PlanCommand(c) => Some(Command::PlanCommand(c)),
            Message::ReviewCommand(c) => Some(Command::ReviewCommand(c)),
            Message::SessionCommand(c) => Some(Command::SessionCommand(c)),
            Message::HostCommand(c) => Some(Command::HostCommand(c)),
            _ => None,
        }
    }

    pub fn into_message(self) -> Message {
        match self {
            Command::ApprovalDecide(c) => Message::ApprovalDecide(c),
            Command::PlanCommand(c) => Message::PlanCommand(c),
            Command::ReviewCommand(c) => Message::ReviewCommand(c),
            Command::SessionCommand(c) => Message::SessionCommand(c),
            Command::HostCommand(c) => Message::HostCommand(c),
        }
    }

    pub fn op(&self) -> &str {
        match self {
            Command::ApprovalDecide(c) => &c.op,
            Command::PlanCommand(c) => &c.op,
            Command::ReviewCommand(c) => &c.op,
            Command::SessionCommand(c) => &c.op,
            Command::HostCommand(c) => &c.op,
        }
    }

    /// What a client must be granted to send it.
    pub fn grant(&self) -> Grant {
        match self {
            Command::ApprovalDecide(_) => Grant::Approve,
            Command::PlanCommand(_) => Grant::Plan,
            Command::ReviewCommand(_) => Grant::Review,
            Command::SessionCommand(_) => Grant::Session,
            Command::HostCommand(_) => Grant::Host,
        }
    }

    /// Bounds a decoded command must keep: identifier and text lengths, the task count, and the
    /// fields an action needs.
    pub fn validate(&self) -> Result<(), WireError> {
        match self {
            Command::ApprovalDecide(c) => c.validate(),
            Command::PlanCommand(c) => c.validate(),
            Command::ReviewCommand(c) => c.validate(),
            Command::SessionCommand(c) => c.validate(),
            Command::HostCommand(c) => c.validate(),
        }
    }
}

/// The identifiers and texts a command carries, checked against their limits together.
struct Bounds<'a> {
    ids: Vec<(&'static str, &'a str)>,
    texts: Vec<(&'static str, &'a str)>,
}

impl<'a> Bounds<'a> {
    fn new(op: &'a str) -> Self {
        Self { ids: vec![("op", op)], texts: Vec::new() }
    }

    fn id(&mut self, what: &'static str, value: Option<&'a str>) -> &mut Self {
        self.ids.extend(value.map(|v| (what, v)));
        self
    }

    fn text(&mut self, what: &'static str, value: Option<&'a str>) -> &mut Self {
        self.texts.extend(value.map(|v| (what, v)));
        self
    }

    fn check(&self) -> Result<(), WireError> {
        if self.ids[0].1.is_empty() {
            return Err(WireError::new(ErrorCode::Malformed, "a mutation needs an operation ID (`op`)"));
        }
        if let Some((what, _)) = self.ids.iter().find(|(_, s)| s.len() > MAX_ID_BYTES) {
            return Err(WireError::new(ErrorCode::Oversize, format!("{what} is too long")));
        }
        if let Some((what, _)) = self.texts.iter().find(|(_, s)| s.len() > MAX_TEXT_BYTES) {
            return Err(WireError::new(ErrorCode::Oversize, format!("{what} is over {MAX_TEXT_BYTES} bytes")));
        }
        Ok(())
    }
}

impl ApprovalDecide {
    pub fn validate(&self) -> Result<(), WireError> {
        Bounds::new(&self.op).id("approval", Some(&self.approval)).text("message", self.message.as_deref()).check()
    }
}

impl PlanCommand {
    pub fn validate(&self) -> Result<(), WireError> {
        let mut bounds = Bounds::new(&self.op);
        bounds.id("plan", self.plan.as_deref()).id("digest", self.digest.as_deref());
        match &self.action {
            PlanAction::Set { goal, tasks, integration } => {
                bounds.text("goal", Some(goal)).id("integration", integration.as_deref());
                if tasks.len() > MAX_TASKS {
                    return Err(WireError::new(
                        ErrorCode::TooMany,
                        format!("tasks: {} exceeds {MAX_TASKS}", tasks.len()),
                    ));
                }
            }
            PlanAction::Dispatch { task } => {
                bounds.id("task", task.as_deref());
            }
            PlanAction::Update { task, note, .. } => {
                bounds.id("task", Some(task)).text("note", note.as_deref());
            }
            PlanAction::Integrate { task } => {
                bounds.id("task", Some(task));
            }
            PlanAction::Notebook { .. } | PlanAction::Approve | PlanAction::Archive | PlanAction::Complete => {}
        }
        bounds.check()?;
        if self.plan.is_none() && !matches!(self.action, PlanAction::Set { .. }) {
            return Err(WireError::new(ErrorCode::Malformed, "name the plan"));
        }
        Ok(())
    }
}

impl ReviewCommand {
    pub fn validate(&self) -> Result<(), WireError> {
        let mut bounds = Bounds::new(&self.op);
        bounds.id("worktree", Some(&self.worktree));
        if let ReviewAction::Add { path, text, .. } | ReviewAction::Remove { path, text, .. } = &self.action {
            bounds.id("path", Some(path)).text("text", Some(text));
        }
        bounds.check()
    }
}

impl SessionCommand {
    pub fn validate(&self) -> Result<(), WireError> {
        let mut bounds = Bounds::new(&self.op);
        match &self.action {
            SessionAction::Steer { session, text } => {
                bounds.id("session", Some(session)).text("text", Some(text));
            }
            SessionAction::Start { agent, folder, repo, branch, prompt } => {
                bounds
                    .id("agent", Some(agent))
                    .id("folder", folder.as_deref())
                    .id("repo", repo.as_deref())
                    .id("branch", branch.as_deref())
                    .text("prompt", prompt.as_deref());
            }
            SessionAction::Close { session } => {
                bounds.id("session", Some(session));
            }
        }
        bounds.check()
    }
}

impl HostCommand {
    pub fn validate(&self) -> Result<(), WireError> {
        let mut bounds = Bounds::new(&self.op);
        match &self.action {
            HostAction::Edit { root, path, .. } => {
                bounds.id("root", Some(root)).id("path", path.as_deref());
            }
            HostAction::EditorHints { root } => {
                bounds.id("root", Some(root));
            }
            HostAction::PullRequestDiff { key } => {
                bounds.id("key", Some(key));
            }
            HostAction::StartChief => {}
            HostAction::SignInPaste { text } => {
                bounds.text("text", Some(text));
            }
            HostAction::OpenWorkspace { name } | HostAction::SaveWorkspace { name } => {
                bounds.id("workspace", Some(name));
            }
            HostAction::Tickets { request, ticket } => {
                if request.trim().is_empty() {
                    return Err(WireError::new(ErrorCode::Malformed, "name the ticket request"));
                }
                bounds.id("request", Some(request));
                use crate::ticket_ui::TicketAction;
                match ticket.as_ref() {
                    TicketAction::Search { query, filters, cursor, .. } => {
                        bounds.text("query", Some(query));
                        bounds.id("cursor", cursor.as_deref());
                        if filters.len() > 64 {
                            return Err(WireError::new(ErrorCode::TooMany, "too many ticket filters"));
                        }
                    }
                    TicketAction::Read { ticket }
                    | TicketAction::Children { ticket }
                    | TicketAction::Related { ticket }
                    | TicketAction::Plan { ticket, .. }
                    | TicketAction::Link { ticket, .. } => {
                        bounds.id("ticket.id", Some(&ticket.id));
                        bounds.id("ticket.system", Some(&ticket.system));
                        if ticket.id.trim().is_empty() {
                            return Err(WireError::new(ErrorCode::Malformed, "name the ticket"));
                        }
                    }
                }
                if let TicketAction::Plan { extra, .. } = ticket.as_ref() {
                    bounds.text("extra", Some(extra));
                    if extra.chars().count() > crate::tickets::PLAN_EXTRA_MAX {
                        return Err(WireError::new(ErrorCode::Oversize, "too many extra instructions"));
                    }
                }
                if let TicketAction::Link { plan, task, .. } = ticket.as_ref() {
                    bounds.id("plan", Some(plan));
                    bounds.id("task", task.as_deref());
                }
            }
            HostAction::BrowseFolderPage { path, .. }
            | HostAction::BrowseFolder { path }
            | HostAction::OpenProject { path }
            | HostAction::CloseProject { path } => {
                bounds.id("path", Some(path));
            }
            HostAction::ConfigPropose { toml, remove, theme, reason, .. } => {
                bounds.text("toml", Some(toml)).text("reason", reason.as_deref()).id("theme", theme.as_deref());
                if remove.len() > MAX_TASKS {
                    return Err(WireError::new(ErrorCode::TooMany, "too many keys"));
                }
                for key in remove {
                    bounds.id("key", Some(key));
                }
            }
            HostAction::ConfigDecide { id, note, .. } => {
                bounds.id("proposal", Some(id)).text("note", Some(note));
            }
            HostAction::SelectTheme { name, .. } => {
                bounds.id("theme", Some(name));
            }
            HostAction::DecidePairing { code, .. } => {
                bounds.id("code", Some(code));
            }
            HostAction::RevokeDevice { id } => {
                bounds.id("device", Some(id));
            }
            HostAction::SettingsTool { tool } => match tool {
                SettingsTool::Connect { name, .. } | SettingsTool::Disconnect { name } => {
                    bounds.id("name", Some(name));
                }
                SettingsTool::AddConnection { name, url } => {
                    bounds.id("name", Some(name)).id("url", Some(url));
                }
                SettingsTool::PutCredential { kind, value, host } => {
                    bounds.id("kind", Some(kind)).id("host", host.as_deref()).text("value", Some(value));
                }
                SettingsTool::RemoveCredential { kind, host } => {
                    bounds.id("kind", Some(kind)).id("host", host.as_deref());
                }
                SettingsTool::InitProject { path } => {
                    bounds.id("path", Some(path));
                }
                SettingsTool::RunSetup { value, .. } => {
                    bounds.text("value", value.as_deref());
                }
                SettingsTool::ConnectLater { id, .. } => {
                    bounds.id("id", Some(id));
                }
                _ => {}
            },
            HostAction::PairDevice | HostAction::CancelPairing => {}
            HostAction::SignIn | HostAction::SignInCancel => {}
        }
        bounds.check()?;
        if let HostAction::OpenWorkspace { name } = &self.action
            && name.trim().is_empty()
        {
            return Err(WireError::new(ErrorCode::Malformed, "name the workspace"));
        }
        Ok(())
    }
}

impl CommandResult {
    pub fn validate(&self) -> Result<(), WireError> {
        Bounds::new(&self.op).check()
    }
}

/// The reference client's operations in flight. It numbers nothing itself: the caller gives each
/// operation a fresh ID (a UUID, say) and the time it was first sent, in milliseconds on any
/// clock that only goes forward.
///
/// After a reconnect, [`Pending::resend`] sends again, with the same IDs, the operations young
/// enough for the host to still remember; their known outcomes come back instead of the work
/// being done twice. Older ones are handed back unsent: whether they took effect is for the user
/// to see on the board, and doing them again needs the user's fresh intent.
#[derive(Debug, Default, Clone)]
pub struct Pending {
    sent: BTreeMap<String, (Command, u64)>,
}

impl Pending {
    /// Track `command` (first sent at `now_ms`) and return the message to send.
    pub fn send(&mut self, command: Command, now_ms: u64) -> Message {
        self.sent.insert(command.op().to_string(), (command.clone(), now_ms));
        command.into_message()
    }

    /// Apply a `command_result`: a final outcome ends the operation and is returned.
    pub fn apply(&mut self, result: &CommandResult) -> Option<Outcome> {
        if !result.outcome.is_final() || self.sent.remove(&result.op).is_none() {
            return None;
        }
        Some(result.outcome.clone())
    }

    /// After a reconnect: the operations to send again, and those too old to, which are dropped.
    pub fn resend(&mut self, now_ms: u64, ttl_secs: u32) -> (Vec<Message>, Vec<Command>) {
        let ttl = u64::from(ttl_secs) * 1000;
        let (fresh, expired): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.sent).into_iter().partition(|(_, (_, at))| now_ms.saturating_sub(*at) < ttl);
        let messages = fresh.iter().map(|(_, (command, _))| command.clone().into_message()).collect();
        self.sent = fresh.into_iter().collect();
        (messages, expired.into_iter().map(|(_, (command, _))| command).collect())
    }

    pub fn is_empty(&self) -> bool {
        self.sent.is_empty()
    }

    pub fn len(&self) -> usize {
        self.sent.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{self, Envelope, Seq};

    fn steer(op: &str) -> Command {
        Command::SessionCommand(SessionCommand {
            op: op.into(),
            action: SessionAction::Steer { session: "a".into(), text: "go on".into() },
        })
    }

    #[test]
    fn mutations_travel_flat_with_their_action() {
        let frame = wire::encode(&Envelope::new(steer("o1").into_message())).unwrap();
        let value: Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(value["kind"], "session_command");
        assert_eq!((value["op"].as_str(), value["action"].as_str()), (Some("o1"), Some("steer")));
        let approve = r#"{"v":{"major":1,"minor":0},"kind":"plan_command","op":"o2","plan":"p","digest":"abc","action":"approve"}"#;
        let Message::PlanCommand(command) = wire::decode(approve).unwrap().message else { panic!("plan_command") };
        assert_eq!((command.plan.as_deref(), command.action), (Some("p"), PlanAction::Approve));
        let add = r#"{"v":{"major":1,"minor":0},"kind":"review_command","op":"o3","worktree":"/w","action":"add","revision":"7","path":"a.rs","line":3,"text":"why?"}"#;
        let Message::ReviewCommand(command) = wire::decode(add).unwrap().message else { panic!("review_command") };
        assert!(matches!(command.action, ReviewAction::Add { revision: Seq(7), line: Some(3), removed: false, .. }));
        let result = Envelope::new(Message::CommandResult(CommandResult {
            op: "o1".into(),
            outcome: Outcome::Refused { code: ErrorCode::AlreadyResolved, message: "allowed".into() },
        }));
        let text = wire::encode(&result).unwrap();
        assert!(text.contains(r#""status":"refused","code":"already_resolved""#), "{text}");
        assert_eq!(wire::decode(&text).unwrap(), result);
    }

    #[test]
    fn a_status_from_a_newer_host_decodes_as_unknown() {
        let frame = r#"{"v":{"major":1,"minor":3},"kind":"command_result","op":"o","status":"queued"}"#;
        let Message::CommandResult(result) = wire::decode(frame).unwrap().message else { panic!("command_result") };
        assert_eq!(result.outcome, Outcome::Unknown);
    }

    #[test]
    fn commands_keep_their_bounds() {
        let ok = steer("o");
        assert!(ok.validate().is_ok());
        let code = |c: Command| c.validate().unwrap_err().code;
        assert_eq!(code(steer("")), ErrorCode::Malformed, "an operation ID is required");
        assert_eq!(code(steer(&"o".repeat(MAX_ID_BYTES + 1))), ErrorCode::Oversize);
        let long = Command::SessionCommand(SessionCommand {
            op: "o".into(),
            action: SessionAction::Steer { session: "a".into(), text: "x".repeat(MAX_TEXT_BYTES + 1) },
        });
        assert_eq!(code(long), ErrorCode::Oversize);
        let unnamed =
            Command::PlanCommand(PlanCommand { op: "o".into(), plan: None, digest: None, action: PlanAction::Approve });
        assert_eq!(code(unnamed), ErrorCode::Malformed, "only set may create a plan");
        let many = Command::PlanCommand(PlanCommand {
            op: "o".into(),
            plan: None,
            digest: None,
            action: PlanAction::Set {
                goal: "g".into(),
                tasks: vec![Task::default(); MAX_TASKS + 1],
                integration: None,
            },
        });
        assert_eq!(code(many), ErrorCode::TooMany);
        // The decoder applies the same bounds.
        let frame =
            wire::encode(&Envelope::new(steer("o").into_message())).unwrap().replace(r#""op":"o""#, r#""op":"""#);
        assert_eq!(wire::decode(&frame).unwrap_err().code, ErrorCode::Malformed);
    }

    #[test]
    fn pending_resends_only_what_the_host_still_remembers() {
        let mut pending = Pending::default();
        pending.send(steer("old"), 0);
        pending.send(steer("young"), 600_000);
        let accepted = CommandResult { op: "young".into(), outcome: Outcome::Accepted };
        assert_eq!(pending.apply(&accepted), None, "accepted is not the end");
        let (again, expired) = pending.resend(u64::from(OP_TTL_SECS) * 1000 + 1, OP_TTL_SECS);
        assert_eq!(again, vec![steer("young").into_message()], "the same ID, so the host answers what it knows");
        assert_eq!(expired, vec![steer("old")], "too old to resend: the user decides again");
        let done = CommandResult { op: "young".into(), outcome: Outcome::done(Value::Null) };
        assert_eq!(pending.apply(&done), Some(Outcome::done(Value::Null)));
        assert!(pending.is_empty());
        assert_eq!(pending.apply(&done), None, "a repeated answer changes nothing");
    }
}
