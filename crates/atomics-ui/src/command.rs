//! What the user asks for. The draw code records commands on the [`Client`](crate::Client) instead of
//! changing the host's model; after the draw the platform carries them out in the order they were
//! recorded: a view command ([`Command::is_view`]) on the UI itself, any other through
//! [`Backend::send`](crate::Backend::send). A plan is addressed like a read, by its position in the last
//! list read; a task by id, or by its position in the plan the board read.
//!
//! Commands are grouped by domain. Approvals, plans, review comments and sessions are the domains of
//! atomics.v1's `approval_decide`, `plan_command`, `review_command` and `session_command`; the ticket
//! sign-in and opening a workspace go as `host_command`; the other project commands, the chief and the
//! settings have no operation there yet.

use std::path::PathBuf;

use atomics_core::approvals::Decision;
use atomics_core::config::ChiefDock;
use atomics_core::ops::{
    self, ApprovalDecide, HostAction, HostCommand, PlanAction, PlanCommand, ReviewAction, ReviewCommand, SessionAction,
    SessionCommand,
};
use atomics_core::plan::{PlanStatus, TaskStatus};
use atomics_core::steer::ReviewComment;
use atomics_core::store::SessionRecord;
use atomics_core::tickets::TicketLink;
use atomics_core::wire::Seq;

use crate::{Backend, PanelMode};

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Open a local server, forwarding it when the host is remote. Also usable by machine views.
    OpenPreview {
        session: Option<String>,
        preview: atomics_core::preview::Preview,
    },
    StopPreview {
        port: u16,
    },
    EditorHints(PathBuf),
    LoadPullRequest(String),
    // ── Approvals ──────────────────────────────────────────────────────────────────────────────────
    /// Answer a permission request of the session at `session` of the last session list read. The request
    /// is named, so the host refuses a decision made for an earlier one.
    Decide {
        session: usize,
        approval: String,
        decision: Decision,
    },

    // ── Plans ────────────────────────────────────────────────────────────────────────────────────────
    /// A planned task's brief, as edited on its card.
    EditBrief {
        plan: usize,
        task: String,
        brief: String,
    },
    CurateNote {
        plan: usize,
        id: u64,
        pinned: Option<bool>,
        delete: bool,
    },
    /// Start a ready task.
    StartTask {
        plan: usize,
        task: String,
    },
    /// Merge a task into the plan's integration branch.
    IntegrateTask {
        plan: usize,
        task: String,
    },
    /// Start a finished task's agent again in its worktree; `task` is its position in the plan.
    ResumeTask {
        plan: usize,
        task: usize,
    },
    /// Dismiss the alert that a task's references moved; `task` is its position in the plan.
    AcknowledgeRefs {
        plan: usize,
        task: usize,
        generation: Option<String>,
    },
    /// Move a task to another column.
    SetTaskStatus {
        plan: usize,
        task: String,
        status: TaskStatus,
    },
    ApprovePlan(usize),
    /// Integrate every accepted task that is not integrated yet.
    IntegrateAccepted(usize),
    /// Read the plan's tickets again now.
    SyncTickets(usize),
    /// Start every task whose dependencies are done.
    DispatchReady(usize),
    ReopenPlan(usize),
    ArchivePlan(usize),
    /// Close a plan as completed or archived, as the user confirmed.
    ClosePlan {
        plan: usize,
        status: PlanStatus,
    },
    /// Archive a plan that is no longer open; one reopened meanwhile stays open.
    /// Desktop only until the protocol supports that execution-time guard.
    ArchiveFinished(usize),
    /// Preview what a completed plan's cleanup would remove.
    CleanUpPlan(usize),
    /// Read the open cleanup preview again.
    ReloadPlanCleanup,
    /// Remove the branches the user picked in the cleanup preview of the completed plan with this id,
    /// with their worktrees; each by its repository and branch.
    RemovePlanBranches {
        plan: String,
        branches: Vec<(PathBuf, String)>,
    },

    // ── Review comments and the diff ─────────────────────────────────────────────────────────────────
    /// The review comments on a worktree's diff changed from `before` to `after`.
    EditComments {
        root: PathBuf,
        before: Vec<ReviewComment>,
        after: Vec<ReviewComment>,
    },
    /// Send a worktree's review comments to the agent working there.
    SendReview(PathBuf),
    /// Read a worktree's diff again now.
    RefreshDiff(PathBuf),
    /// Show a file of a worktree in its diff view: its hunks when changed, the whole file otherwise.
    OpenFile {
        root: PathBuf,
        path: String,
    },
    /// The diff view moved to another file or hunk: the host stops showing a file whole there.
    LeaveFile(PathBuf),
    /// Open a worktree's file, or the worktree itself, in the editor; at `line` if given.
    Edit {
        root: PathBuf,
        path: Option<String>,
        line: Option<u32>,
    },

    // ── Sessions ───────────────────────────────────────────────────────────────────────────────────────
    /// Type a message into each of these sessions, by id, as one steer each; a toast says how it went.
    Steer {
        sessions: Vec<String>,
        text: String,
    },
    /// Start a configured agent in a folder.
    StartAgent {
        agent: String,
        folder: PathBuf,
    },
    /// Start a configured agent as `atomics ctl start` does: in `folder`, or with none in a scratch folder
    /// of its own outside any project. The welcome's buttons start agents this way.
    Start {
        agent: String,
        folder: Option<PathBuf>,
    },
    /// End a session, by id, and close its tab.
    CloseSession(String),
    /// Start a session again from its record.
    Resume(SessionRecord),
    /// Start an exited session, by id, again in its place: its tile closes and its agent picks its
    /// conversation up again.
    ResumeSession(String),
    /// Give a session, by id, a new title.
    RenameSession {
        session: String,
        title: String,
    },
    /// Close a project's idle sessions that no plan task uses.
    CloseIdle(PathBuf),
    /// The user looked at a session, by id: a finished one becomes idle.
    SeeSession(String),
    /// Hide a session's banner, by id, about the MCP servers it needs.
    DismissMcp(String),
    /// What the user said, for the session at `session` of the last session list read.
    VoiceToSession {
        session: usize,
        text: String,
    },

    // ── The ticket system's sign-in ────────────────────────────────────────────────────────────────────
    /// Open the GitHub controls and start connecting; only a host's own window can.
    ConnectGithub,
    /// Sign in to the ticket system, or again after the server refused the saved sign-in: the host's
    /// sign-in job, after which its ticket worker connects again. A desktop host may open its browser.
    SignInTickets,
    Ticket {
        request: String,
        action: atomics_core::ticket_ui::TicketAction,
    },
    /// The address a browser ended on after signing in, for the sign-in under way.
    PasteSignIn(String),
    /// Stop the sign-in under way.
    CancelSignIn,

    // ── Projects and worktrees ───────────────────────────────────────────────────────────────────────
    Manage(HostAction),
    OpenProject(PathBuf),
    CloseProject(PathBuf),
    /// Open a saved workspace, by name, in place of the open projects.
    OpenWorkspace(String),
    /// Save the open projects as a workspace.
    SaveWorkspace(String),
    /// Look for merged or abandoned worktrees of a project, with one picked already if given.
    OpenCleanup {
        repo: PathBuf,
        preselect: Option<PathBuf>,
    },
    /// In the open worktree cleanup: `git fetch --prune`, to learn which remote branches are gone.
    CleanupFetch,
    /// In the open worktree cleanup: forget worktrees whose folder no longer exists.
    CleanupPrune,
    /// Read the open worktree cleanup's list again.
    CleanupReload,
    /// Remove these worktrees of the open cleanup, each with its sandboxes, and these sandboxes whose
    /// worktree is gone; with the worktrees' branches if `delete_branches`.
    RemoveWorktrees {
        worktrees: Vec<PathBuf>,
        orphans: Vec<String>,
        delete_branches: bool,
    },

    // ── The chief and settings ─────────────────────────────────────────────────────────────────────────
    /// Show or hide the chief, starting it when it is not running.
    ToggleChief,
    /// Float or dock the chief's window, and remember it.
    SetChiefDock(ChiefDock),
    /// Save the chief's window preferences: its dock and width.
    SaveChiefPreferences,
    /// What the user said, for the chief, which starts with it as its first prompt when it is not running.
    VoiceToChief(String),
    /// Open the config file in an editor of the machine this UI runs on: only a window on the host has one.
    EditConfig,
    /// Check again whether agents can run in Docker Sandboxes.
    CheckSandbox,
    /// Install the newer version, or, once installed, restart into it when agents finish. Only a window on the
    /// host offers this: atomics.v1 has no operation for it.
    Update,
    /// Stop waiting to restart into the installed version.
    CancelUpdate,
    /// Read the MCP servers sbx has and ATOMICS knows again.
    RefreshConnections,
    /// Import the MCP servers the agents' own configs name.
    ImportConnections,
    /// Connect an MCP server: sbx's registration, else its definition, else `url`; its environment is
    /// stored first with `store_env`.
    Connect {
        name: String,
        store_env: bool,
        url: Option<String>,
    },
    Disconnect(String),
    /// Add a remote MCP server by name and URL.
    AddConnection {
        name: String,
        url: String,
    },
    /// Apply the chief's config proposal with this id, or reject it, with the user's note for the chief.
    DecideProposal {
        id: String,
        apply: bool,
        note: String,
    },

    // ── View: the UI's own, never sent to a host ───────────────────────────────────────────────────────
    /// Make a session the active one, opening the diff panel if `open_diff`.
    Activate {
        session: String,
        open_diff: bool,
    },
    /// Show a task's session, with its diff.
    OpenTaskSession(String),
    /// Show a finished task's worktree in the diff panel; `task` is its position in the plan.
    ShowTaskDiff {
        plan: usize,
        task: usize,
    },
    /// Show the diff of a worktree or main checkout no session is active in.
    ShowRoot(PathBuf),
    /// Show the diff panel.
    OpenDiff,
    /// Hide the diff panel.
    CloseDiff,
    /// Show or hide the diff panel.
    ToggleDiff,
    /// Show the diff, the editor, the plan or the tickets in the diff panel.
    SetPanel(PanelMode),
    /// Show every project's sessions, or one project's.
    SetView(Option<PathBuf>),
    /// Show the sessions outside any plan.
    ShowUngrouped,
    /// Show an open plan's sessions and board.
    ShowPlan(usize),
    /// Show a finished plan's board, read-only.
    ViewPlanHistory(usize),
    /// Show or hide the plan board.
    TogglePlan,
    /// The view switcher's segment: show the board, the diff (or the editor) or the tickets in the panel, or
    /// hide the panel when it shows that view already.
    ToggleView(PanelMode),
    /// Read a ticket in the app's ticket reader. A platform without one does not offer this.
    OpenTicket(TicketLink),
    /// A task brief on the board has the keyboard this frame: the desktop's host holds off a restart until
    /// the edit is done.
    EditingPlan,
    /// The close of a board's row: an open plan asks to be closed first, a finished one is archived
    /// ([`Command::ArchiveFinished`]).
    ClosePlanTab(usize),
    /// Open the new-session dialog.
    NewSession,
    /// Ask where to create a new worktree of a project.
    NewWorktree(PathBuf),
    /// Ask for a session's new title.
    Rename(String),
    /// Ask the platform for a folder to open as a project.
    PickProject,
    /// Hide the chief's window; the chief keeps running.
    HideChief,
    /// Give the docked chief this width, or its default with `None`.
    ResizeChief(Option<f32>),
    /// Say something short on the notice line.
    Toast(String),
}

impl Command {
    /// Whether the command is the UI's own: what its window shows, or a dialog it opens. The platform
    /// carries it out itself; it never reaches [`Backend::send`](crate::Backend::send). The desktop keeps
    /// these view hints in its host's model too, where ctl reads them.
    pub fn is_view(&self) -> bool {
        matches!(
            self,
            Command::Activate { .. }
                | Command::OpenTaskSession(_)
                | Command::ShowTaskDiff { .. }
                | Command::ShowRoot(_)
                | Command::OpenDiff
                | Command::CloseDiff
                | Command::ToggleDiff
                | Command::SetPanel(_)
                | Command::SetView(_)
                | Command::ShowUngrouped
                | Command::ShowPlan(_)
                | Command::ViewPlanHistory(_)
                | Command::TogglePlan
                | Command::ToggleView(_)
                | Command::OpenTicket(_)
                | Command::EditingPlan
                | Command::ClosePlanTab(_)
                | Command::NewSession
                | Command::NewWorktree(_)
                | Command::Rename(_)
                | Command::PickProject
                | Command::HideChief
                | Command::ResizeChief(_)
                | Command::Toast(_)
                | Command::ConnectGithub
        )
    }

    /// What a client of a remote host does with this command: the atomics.v1 mutations to send, if it has
    /// any. Positions are read from `backend`, the client's last snapshot; `op` gives each mutation its
    /// operation ID. Approval, plan, review and session commands map onto `approval_decide`,
    /// `plan_command`, `review_command` and `session_command`; a command whose action atomics.v1 lacks
    /// is [`Remote::Unsupported`].
    pub fn remote(&self, backend: &dyn Backend, op: &mut dyn FnMut() -> String) -> Remote {
        if self.is_view() {
            return Remote::Client;
        }
        let plan = |index: usize, action: PlanAction, op: &mut dyn FnMut() -> String| {
            let row = backend.plan_row(index)?;
            let digest = if matches!(action, PlanAction::Approve) { backend.plan_digest(index) } else { None };
            Some(ops::Command::PlanCommand(PlanCommand { op: op(), plan: Some(row.id), digest, action }))
        };
        let review = |root: &PathBuf, action: ReviewAction, op: &mut dyn FnMut() -> String| {
            let worktree = root.to_string_lossy().into_owned();
            ops::Command::ReviewCommand(ReviewCommand { op: op(), worktree, action })
        };
        let session = |action: SessionAction, op: &mut dyn FnMut() -> String| {
            ops::Command::SessionCommand(SessionCommand { op: op(), action })
        };
        let host = |action: HostAction, op: &mut dyn FnMut() -> String| {
            vec![ops::Command::HostCommand(HostCommand { op: op(), action })]
        };
        let sent = match self {
            Command::Edit { root, path, line } => host(
                HostAction::Edit { root: root.to_string_lossy().into_owned(), path: path.clone(), line: *line },
                op,
            ),
            Command::EditorHints(root) => {
                host(HostAction::EditorHints { root: root.to_string_lossy().into_owned() }, op)
            }
            Command::LoadPullRequest(key) => host(HostAction::PullRequestDiff { key: key.clone() }, op),
            Command::ToggleChief => host(HostAction::StartChief, op),
            Command::Decide { approval, decision, .. } => vec![ops::Command::ApprovalDecide(ApprovalDecide {
                op: op(),
                approval: approval.clone(),
                decision: *decision,
                message: None,
            })],
            Command::ApprovePlan(index) => plan(*index, PlanAction::Approve, op).into_iter().collect(),
            Command::CurateNote { plan: index, id, pinned, delete } => {
                plan(*index, PlanAction::Notebook { id: *id, pinned: *pinned, delete: *delete }, op)
                    .into_iter()
                    .collect()
            }
            Command::StartTask { plan: index, task } => {
                plan(*index, PlanAction::Dispatch { task: Some(task.clone()) }, op).into_iter().collect()
            }
            Command::DispatchReady(index) => {
                plan(*index, PlanAction::Dispatch { task: None }, op).into_iter().collect()
            }
            Command::SetTaskStatus { plan: index, task, status } => {
                let action = PlanAction::Update { task: task.clone(), status: Some(*status), note: None };
                plan(*index, action, op).into_iter().collect()
            }
            Command::IntegrateTask { plan: index, task } => {
                plan(*index, PlanAction::Integrate { task: task.clone() }, op).into_iter().collect()
            }
            Command::IntegrateAccepted(index) => {
                let Some(board) = backend.board(*index) else { return Remote::Send(Vec::new()) };
                let tasks: Vec<String> = board.plan.pending_integrations().iter().map(|t| t.id.clone()).collect();
                tasks.into_iter().filter_map(|task| plan(*index, PlanAction::Integrate { task }, op)).collect()
            }
            Command::ArchivePlan(index) | Command::ClosePlan { plan: index, status: PlanStatus::Archived } => {
                plan(*index, PlanAction::Archive, op).into_iter().collect()
            }
            Command::ClosePlan { plan: index, status: PlanStatus::Completed } => {
                plan(*index, PlanAction::Complete, op).into_iter().collect()
            }
            Command::EditComments { root, before, after } => {
                let Some(revision) = backend.diff(root).map(|diff| Seq(diff.revision)) else {
                    return Remote::Send(Vec::new());
                };
                let mut added = after.clone();
                let mut removed = Vec::new();
                for comment in before {
                    match added.iter().position(|kept| kept == comment) {
                        Some(at) => {
                            added.remove(at);
                        }
                        None => removed.push(comment),
                    }
                }
                let remove = removed.into_iter().map(|c| ReviewAction::Remove {
                    path: c.path.clone(),
                    line: c.line,
                    removed: c.removed,
                    text: c.body.clone(),
                });
                let add = added.into_iter().map(|c| ReviewAction::Add {
                    revision,
                    path: c.path,
                    line: c.line,
                    removed: c.removed,
                    text: c.body,
                });
                remove.chain(add).map(|action| review(root, action, op)).collect()
            }
            Command::SendReview(root) => vec![review(root, ReviewAction::Send, op)],
            Command::Steer { sessions, text } => sessions
                .iter()
                .map(|id| session(SessionAction::Steer { session: id.clone(), text: text.clone() }, op))
                .collect(),
            Command::VoiceToSession { session: index, text } => backend
                .session_row(*index)
                .map(|row| session(SessionAction::Steer { session: row.id, text: text.clone() }, op))
                .into_iter()
                .collect(),
            Command::StartAgent { agent, folder } => {
                let folder = Some(folder.to_string_lossy().into_owned());
                let action =
                    SessionAction::Start { agent: agent.clone(), folder, repo: None, branch: None, prompt: None };
                vec![session(action, op)]
            }
            Command::Start { agent, folder } => {
                let folder = folder.as_ref().map(|folder| folder.to_string_lossy().into_owned());
                let action =
                    SessionAction::Start { agent: agent.clone(), folder, repo: None, branch: None, prompt: None };
                vec![session(action, op)]
            }
            Command::CloseSession(id) => vec![session(SessionAction::Close { session: id.clone() }, op)],
            Command::Ticket { request, action } => {
                host(HostAction::Tickets { request: request.clone(), ticket: Box::new(action.clone()) }, op)
            }
            Command::SignInTickets => host(HostAction::SignIn, op),
            Command::PasteSignIn(text) => host(HostAction::SignInPaste { text: text.clone() }, op),
            Command::CancelSignIn => host(HostAction::SignInCancel, op),
            Command::OpenWorkspace(name) => host(HostAction::OpenWorkspace { name: name.clone() }, op),
            Command::Manage(action) => host(action.clone(), op),
            Command::OpenProject(path) => {
                host(HostAction::OpenProject { path: path.to_string_lossy().into_owned() }, op)
            }
            Command::CloseProject(path) => {
                host(HostAction::CloseProject { path: path.to_string_lossy().into_owned() }, op)
            }
            Command::SaveWorkspace(name) => host(HostAction::SaveWorkspace { name: name.clone() }, op),
            Command::DecideProposal { id, apply, note } => {
                host(HostAction::ConfigDecide { id: id.clone(), apply: *apply, note: note.clone() }, op)
            }
            _ => return Remote::Unsupported,
        };
        Remote::Send(sent)
    }

    /// A task card's `diff` or `resume` button, for the task at position `task` of the
    /// plan at `plan`; `None` for any other action.
    pub fn task(plan: usize, task: usize, action: &str) -> Option<Command> {
        match action {
            "diff" => Some(Command::ShowTaskDiff { plan, task }),
            "resume" => Some(Command::ResumeTask { plan, task }),
            _ => None,
        }
    }
}

/// What a client of a remote host does with a [`Command`] ([`Command::remote`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Remote {
    /// Send these atomics.v1 mutations, in order. Empty when what the command names is gone.
    Send(Vec<ops::Command>),
    /// A view command: the client carries it out itself.
    Client,
    /// atomics.v1 has no operation for it yet, so a remote client does not offer it.
    Unsupported,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_shortcuts_cannot_acknowledge_without_an_alert_generation() {
        assert!(Command::task(0, 0, "acknowledge_refs").is_none());
        assert!(matches!(Command::task(0, 0, "resume"), Some(Command::ResumeTask { .. })));
    }
}
