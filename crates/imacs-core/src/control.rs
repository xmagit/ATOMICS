//! Remote control of a running IMACS over its socket: what `imacs ctl` and the MCP
//! server speak. One JSON object per line in each direction.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::diff::{FileStatus, LineKind, WorktreeDiff};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum ControlRequest {
    List,
    /// Starts an agent in `folder`, or in a new worktree of `repo` on `branch`.
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
    },
    /// Types a message and submits it, or with `raw` writes the text as-is (keys).
    Send {
        session: String,
        text: String,
        #[serde(default)]
        raw: bool,
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
    /// Replaces the plan (tasks keep their progress when their id stays). The user
    /// has to approve it on the board before anything is dispatched.
    PlanSet {
        goal: String,
        tasks: Vec<crate::plan::Task>,
        /// Branch the tasks are merged into; kept while the goal stays the same.
        #[serde(default)]
        integration: Option<String>,
    },
    PlanGet,
    /// Changes a task's status or adds a note to it.
    PlanUpdate {
        id: String,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        note: Option<String>,
        /// Links the task to another agent session (after restarting its agent).
        #[serde(default)]
        session: Option<String>,
    },
    /// Starts the given task, or every ready task, each in a new worktree.
    PlanDispatch {
        #[serde(default)]
        id: Option<String>,
    },
    /// Commits a task's uncommitted work on its branch and merges the branch into the
    /// plan's integration branch, in a worktree of its own.
    PlanIntegrate {
        id: String,
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
    ShowDiff {
        open: bool,
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
        assert!(matches!(start, ControlRequest::Start { args, .. } if args.is_empty()));
        let integrate: ControlRequest = serde_json::from_str(r#"{"cmd":"plan_integrate","id":"t1"}"#).unwrap();
        assert!(matches!(integrate, ControlRequest::PlanIntegrate { id } if id == "t1"));
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
