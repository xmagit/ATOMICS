//! Remote control of a running IMACS over its socket: what `imacs ctl` and the MCP
//! server speak. One JSON object per line in each direction.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::diff::{FileStatus, LineKind, WorktreeDiff};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum ControlRequest {
    List,
    /// Restart the same installation after saving state. Force bypasses busy checks only.
    Restart {
        #[serde(default)]
        force: bool,
        #[serde(default)]
        reason: Option<String>,
    },
    /// Starts an agent in `folder`, in a new worktree of `repo` on `branch`, or without either
    /// in a scratch folder of its own outside any project. `isolation` overrides the settings for it.
    Start {
        agent: String,
        #[serde(default)]
        folder: Option<String>,
        #[serde(default)]
        repo: Option<String>,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        isolation: Option<crate::isolation::IsolationMode>,
    },
    /// Types a message and submits it, or with `raw` writes the text as-is (keys).
    Send {
        session: String,
        text: String,
        #[serde(default)]
        raw: bool,
    },
    Approve {
        session: String,
        decision: crate::approvals::Decision,
        #[serde(default)]
        message: Option<String>,
    },
    /// The visible terminal text, last `lines` lines.
    Read {
        session: String,
        #[serde(default)]
        lines: Option<usize>,
    },
    Diff {
        session: String,
        #[serde(default)]
        patch: bool,
    },
    /// Runs a shell command in a sandboxed session's sandbox, in its worktree, once a slot of
    /// `class` (default test) is free: the chief's checks of agent code, kept off this machine.
    Exec {
        session: String,
        command: String,
        /// How long the command may run, and how long it waits for a slot first.
        #[serde(default)]
        timeout_secs: Option<u64>,
        #[serde(default)]
        class: Option<crate::governor::SlotClass>,
    },
    /// Adds a review comment on a line of the session's diff; it shows in the diff
    /// panel until sent with `send_review`.
    Comment {
        session: String,
        path: String,
        #[serde(default)]
        line: Option<u32>,
        /// The line number refers to a removed line (old file).
        #[serde(default)]
        removed: bool,
        text: String,
    },
    /// Sends all pending review comments for the session's worktree as one message.
    SendReview {
        session: String,
    },
    Activate {
        session: String,
    },
    Close {
        session: String,
    },
    Screenshot {
        path: String,
    },
    /// Opens a file of the session's worktree in the diff panel: its hunks when
    /// changed, the whole file otherwise.
    Open {
        session: String,
        path: String,
    },
    /// Opens a file of the session's worktree in the editor, at a line.
    Edit {
        session: String,
        path: String,
        #[serde(default)]
        line: Option<u32>,
    },
    /// Blocks until a session reaches one of `statuses` (default: turn done, needs you,
    /// exited) after event `after`, or `timeout_secs` pass. Empty `sessions` = any.
    Wait {
        #[serde(default)]
        sessions: Vec<String>,
        #[serde(default)]
        statuses: Vec<String>,
        #[serde(default)]
        after: Option<u64>,
        #[serde(default)]
        timeout_secs: Option<u64>,
    },
    /// What a session produced: status, last prompt, final message, screen tail, diff size.
    Result {
        session: String,
    },
    /// Creates a separate plan without an id, or updates that plan with one. Tasks
    /// keep progress by task id, including when rewording the goal; the user approves before dispatch.
    PlanSet {
        #[serde(default)]
        plan: Option<String>,
        goal: String,
        tasks: Vec<crate::plan::Task>,
        /// Branch the tasks are merged into; kept unless explicitly overridden.
        #[serde(default)]
        integration: Option<String>,
        /// The plan's board or ticket; kept when omitted.
        #[serde(default)]
        ticket: Option<crate::tickets::TicketLink>,
    },
    /// Removes an open plan from the tabs, retaining its tasks and history.
    PlanClose {
        plan: String,
        status: crate::plan::PlanStatus,
    },
    PlanGet {
        #[serde(default)]
        plan: Option<String>,
    },
    /// Approves a reviewed plan so it can dispatch tasks, as the board's Approve action does.
    PlanApprove {
        #[serde(default)]
        plan: Option<String>,
    },
    /// Changes a task's status or adds a note to it.
    PlanUpdate {
        #[serde(default)]
        plan: Option<String>,
        id: String,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        note: Option<String>,
        /// Links the task to another agent session (after restarting its agent).
        #[serde(default)]
        session: Option<String>,
    },
    /// Report a lifecycle milestone as chief, or from a webhook bridge via stage_event.
    PlanStage {
        plan: String,
        #[serde(default)]
        task: Option<String>,
        stage: String,
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        note: Option<String>,
    },
    StageEvent {
        #[serde(default)]
        plan: Option<String>,
        #[serde(default)]
        task: Option<String>,
        #[serde(default)]
        system: Option<String>,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        key: Option<String>,
        stage: String,
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        note: Option<String>,
    },
    /// Starts the given task, or every ready task, each in a new worktree.
    PlanDispatch {
        #[serde(default)]
        plan: Option<String>,
        #[serde(default)]
        id: Option<String>,
    },
    /// Commits a task's uncommitted work on its branch and merges the branch into the
    /// plan's integration branch, in a worktree of its own.
    PlanIntegrate {
        #[serde(default)]
        plan: Option<String>,
        id: String,
    },
    /// Attaches a ticket to a plan (without `task`) or a task, creates one through the
    /// configured create operation, or with `unlink` removes the link.
    TicketLink {
        #[serde(default)]
        plan: Option<String>,
        #[serde(default)]
        task: Option<String>,
        #[serde(default)]
        ticket: Option<crate::tickets::TicketLink>,
        #[serde(default)]
        create: Option<crate::tickets::TicketDraft>,
        #[serde(default)]
        unlink: bool,
    },
    /// Reads the linked tickets again and retries failed pushes; answers when done.
    TicketSync {
        #[serde(default)]
        plan: Option<String>,
    },
    /// Comments on the task's ticket (or the plan's ticket in ticket mode) as the chief.
    TicketComment {
        #[serde(default)]
        plan: Option<String>,
        #[serde(default)]
        task: Option<String>,
        text: String,
    },
    /// An external system or webhook bridge saying a ticket (or a board) changed.
    /// Given fields apply at once; the linked tickets are then read again.
    TicketEvent {
        #[serde(default)]
        system: Option<String>,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        key: Option<String>,
        #[serde(default)]
        board: Option<String>,
        #[serde(default)]
        state: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        updated_at: Option<String>,
    },
    /// Read only the session's task ticket or its plan link through operations.get.
    AgentTicketGet {
        pane: Option<String>,
        #[serde(default)]
        plan: bool,
    },
    /// Comment only on the session's task ticket through operations.comment.
    AgentTicketComment {
        pane: Option<String>,
        text: String,
    },
    /// A sandboxed agent asks for an MCP server it needs: a connected one is loaded into its
    /// sandbox, any other gets a "Connect" banner on its tile (sent by `imacs mcp --agent`).
    McpConnect {
        pane: Option<String>,
        server: String,
        /// The server's https MCP URL, for one IMACS does not know yet.
        #[serde(default)]
        url: Option<String>,
    },
    /// The effective configuration with each value's source, without secrets: the main
    /// config, the active workspace, and one project when given.
    ConfigGet {
        #[serde(default)]
        project: Option<String>,
        /// Only keys under this dotted prefix, such as `tickets`.
        #[serde(default)]
        key: Option<String>,
    },
    /// The doctor's checks as JSON, without secrets, for the active workspace.
    Doctor,
    /// A change to one config file, shown to the user as a diff to apply or reject. Nothing
    /// is written until the user applies it; the outcome arrives as a `config` event.
    ConfigPropose {
        scope: crate::selfconfig::Scope,
        /// Merged into the file: tables key by key, other values replace.
        #[serde(default)]
        toml: String,
        /// Dotted keys taken out of the file first.
        #[serde(default)]
        remove: Vec<String>,
        /// With scope `theme`: the theme's name; the file is `themes/<theme>.toml`.
        #[serde(default)]
        theme: Option<String>,
        /// Why, shown to the user with the diff.
        #[serde(default)]
        reason: Option<String>,
        /// The proposing session, set by `imacs mcp`.
        #[serde(default)]
        pane: Option<String>,
    },
    /// A sub-agent reporting on its task (sent by `imacs mcp --agent`).
    Report {
        pane: Option<String>,
        status: String,
        summary: String,
    },
    /// Toggles inlay hints in the editor of the session's worktree.
    InlayHints {
        session: String,
    },
    /// Adds a project (any folder inside a git repository) to the explorer.
    OpenProject {
        path: String,
    },
    /// Opens the worktree clean-up dialog for a project.
    Cleanup {
        project: String,
    },
    /// Shows or hides the diff panel.
    /// Open or close the Settings menu, with its theme picker, in the window.
    ShowSettings {
        open: bool,
    },
    ShowDiff {
        open: bool,
    },
    /// Shows or hides the chief, optionally switching its presentation or width.
    ShowChief {
        open: bool,
        #[serde(default)]
        dock: Option<crate::config::ChiefDock>,
        #[serde(default)]
        width: Option<f32>,
    },
    /// Resizes the window (in points), to try the responsive layout.
    Resize {
        width: f32,
        height: f32,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ControlResponse {
    pub ok: bool,
    #[serde(default)]
    pub result: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ControlResponse {
    pub fn ok(result: Value) -> ControlResponse {
        ControlResponse { ok: true, result, error: None }
    }

    pub fn error(message: impl Into<String>) -> ControlResponse {
        ControlResponse { ok: false, result: Value::Null, error: Some(message.into()) }
    }
}

const MAX_PATCH_BYTES: usize = 120_000;

/// A plain-text rendering of a worktree diff for agents: a file summary, and
/// with `patch` the hunks themselves.
pub fn describe_diff(diff: &WorktreeDiff, patch: bool) -> String {
    let short = &diff.base[..diff.base.len().min(10)];
    let mut out = format!(
        "{} files changed, +{} -{} against {} (merge-base {short})\n",
        diff.files.len(),
        diff.additions(),
        diff.deletions(),
        diff.base_ref
    );
    for file in &diff.files {
        let status = match file.status {
            FileStatus::Added => "added",
            FileStatus::Untracked => "untracked",
            FileStatus::Deleted => "deleted",
            FileStatus::Renamed => "renamed",
            FileStatus::Modified => "modified",
        };
        out.push_str(&format!("  {} (+{} -{}, {status})\n", file.path(), file.additions(), file.deletions()));
    }
    if !patch {
        return out;
    }
    for file in &diff.files {
        out.push_str(&format!("\n--- {}\n", file.path()));
        if file.binary {
            out.push_str("(binary)\n");
        }
        for hunk in &file.hunks {
            out.push_str(&hunk.header);
            out.push('\n');
            for line in &hunk.lines {
                let sign = match line.kind {
                    LineKind::Added => '+',
                    LineKind::Removed => '-',
                    LineKind::Context => ' ',
                    LineKind::Note => '\\',
                };
                out.push(sign);
                out.push_str(&line.text);
                out.push('\n');
            }
        }
        if out.len() > MAX_PATCH_BYTES {
            let mut cut = MAX_PATCH_BYTES;
            while !out.is_char_boundary(cut) {
                cut -= 1;
            }
            out.truncate(cut);
            out.push_str("\n… (truncated)\n");
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::FileDiff;

    #[test]
    fn requests_round_trip_with_defaults() {
        let request: ControlRequest = serde_json::from_str(r#"{"cmd":"send","session":"abc","text":"hi"}"#).unwrap();
        assert!(matches!(request, ControlRequest::Send { raw: false, .. }));
        let start: ControlRequest = serde_json::from_str(r#"{"cmd":"start","agent":"codex","folder":"/tmp"}"#).unwrap();
        assert!(matches!(start, ControlRequest::Start { args, isolation: None, .. } if args.is_empty()));
        let scratch: ControlRequest =
            serde_json::from_str(r#"{"cmd":"start","agent":"claude","isolation":"bare"}"#).unwrap();
        assert!(matches!(
            scratch,
            ControlRequest::Start {
                folder: None,
                repo: None,
                isolation: Some(crate::isolation::IsolationMode::Bare),
                ..
            }
        ));
        let integrate: ControlRequest = serde_json::from_str(r#"{"cmd":"plan_integrate","id":"t1"}"#).unwrap();
        assert!(matches!(integrate, ControlRequest::PlanIntegrate { id, plan: None } if id == "t1"));
    }

    #[test]
    fn describes_diff_with_patch() {
        let diff = WorktreeDiff {
            base_ref: "main".into(),
            base: "0123456789abcdef".into(),
            files: vec![FileDiff::untracked("a.py".into(), "print(1)\n")],
            partial: false,
        };
        let text = describe_diff(&diff, true);
        assert!(text.starts_with("1 files changed, +1 -0 against main (merge-base 0123456789)"));
        assert!(text.contains("  a.py (+1 -0, untracked)"));
        assert!(text.contains("+print(1)"));
    }
}
