//! Remote control of a running ATOMICS over its socket: what `atomics ctl` and the MCP
//! server speak. One JSON object per line in each direction.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::diff::{FileStatus, LineKind, WorktreeDiff};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum ControlRequest {
    Preview {
        #[serde(default)]
        pane: Option<String>,
        port: u16,
        #[serde(default)]
        path: String,
        #[serde(default)]
        label: String,
        #[serde(default)]
        remove: bool,
    },
    List,
    /// The protocol server's address and the rendezvous's status: phase, registered, online,
    /// channels and why it waits. Never a token or pass.
    Remote,
    /// Pair a client that reaches the protocol server through a forward (a Mac's ATOMICS reaching
    /// the host in its VM): its token, once, in the answer; pairing a name again replaces it.
    PairClient {
        name: String,
    },
    /// Forget a client paired as `name`; its token stops working.
    UnpairClient {
        name: String,
    },
    /// The chief's cleaned MCP calls, oldest first among the newest `limit` entries.
    TraceGet {
        #[serde(default)]
        session: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// Restart the same installation after saving state. Force bypasses busy checks only.
    Restart {
        #[serde(default)]
        force: bool,
        #[serde(default)]
        reason: Option<String>,
        /// The session asking, when its own MCP server sends this: a chief's own turn does not hold back the restart
        /// it asked for.
        #[serde(default)]
        pane: Option<String>,
    },
    /// `atomics update` installed a new version from a terminal: restart when agents finish, as the Update
    /// button does. It never forces.
    UpdateInstalled {
        version: String,
        /// The program that was updated: a host running another one has nothing to restart into.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        program: Option<String>,
    },
    /// An `atomics://open` link, parsed by the launching process: focus the window and show a
    /// plan, or the plan for a ticket. `rejected` carries why a link was refused, for a toast.
    OpenLink {
        #[serde(default)]
        plan: Option<String>,
        #[serde(default)]
        ticket: Option<String>,
        #[serde(default)]
        system: Option<String>,
        #[serde(default)]
        rejected: Option<String>,
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
        /// A person pressed Start: if no sandbox can be had for it, ask them what to do instead of failing.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        interactive: bool,
    },
    /// Starts the chief if it is not running, without opening a window.
    StartChief,
    /// Types a message and submits it, or with `raw` writes the text as-is (keys).
    /// Addressing `chief` without `raw` starts it with this first prompt when needed.
    Send {
        session: String,
        text: String,
        #[serde(default)]
        raw: bool,
    },
    Approve {
        session: String,
        /// The id of the pending request this decision answers, as `list`, `result` and `wait` show it.
        /// Without it, or with one no longer pending, nothing is answered.
        #[serde(default)]
        request: Option<String>,
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
    /// The worktree's diff summary; with `patch` its hunks, up to [`MAX_PATCH_CHARS`]. `path`
    /// keeps only that file, or the files under that folder.
    Diff {
        session: String,
        #[serde(default)]
        patch: bool,
        #[serde(default)]
        path: Option<String>,
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
        /// Compact events and only sessions with status changes. MCP enables this; ctl keeps its legacy default.
        #[serde(default)]
        compact: bool,
        /// Include every session even in a compact answer.
        #[serde(default)]
        full_sessions: bool,
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
    /// Without `plan`, the open plans' summaries. With `plan`, a compact view of every task that
    /// stays under [`MAX_PLAN_VIEW_CHARS`]; with `task` too, that one task in full.
    PlanGet {
        #[serde(default)]
        plan: Option<String>,
        #[serde(default)]
        task: Option<String>,
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
    /// Chief-only ticket discovery; results are cleaned cards, capped at 25.
    TicketSearch {
        query: String,
        #[serde(default)]
        semantic: bool,
        #[serde(default)]
        take: Option<usize>,
    },
    /// Chief-only full reading card, through read or the get fallback.
    TicketRead {
        ticket: crate::tickets::TicketReference,
    },
    TicketChildren {
        ticket: crate::tickets::TicketReference,
    },
    TicketRelated {
        ticket: crate::tickets::TicketReference,
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
    /// Propose a fact to the host-resolved task's own plan. Never directly shared with peers.
    Note {
        pane: Option<String>,
        text: String,
    },
    /// Read only the pinned facts of the host-resolved task's plan.
    Notes {
        pane: Option<String>,
    },
    /// Chief-only proposal review and curation; user actions also use Plan::curate_note.
    PlanNotebook {
        pane: Option<String>,
        #[serde(default)]
        plan: Option<String>,
        #[serde(default)]
        id: Option<u64>,
        #[serde(default)]
        pinned: Option<bool>,
        #[serde(default)]
        delete: bool,
    },
    /// User curation from the board or authenticated protocol. Not exposed to task MCP clients.
    CurateNote {
        plan: String,
        id: u64,
        pinned: Option<bool>,
        delete: bool,
    },
    /// A sandboxed agent asks for an MCP server it needs: a connected one is loaded into its
    /// sandbox, any other gets a "Connect" banner on its tile (sent by `atomics mcp --agent`).
    McpConnect {
        pane: Option<String>,
        server: String,
        /// The server's https MCP URL, for one ATOMICS does not know yet.
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
        /// The proposing session, set by `atomics mcp`.
        #[serde(default)]
        pane: Option<String>,
    },
    /// A sub-agent reporting on its task (sent by `atomics mcp --agent`).
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

// What an MCP client accepts from one tool call: Claude Code gives a call 120 s and caps its
// text at about 25,000 tokens. Chief tool answers stay under both.

/// The longest a `wait` blocks, and its default: well inside the client's 120 s for a call.
pub const MAX_WAIT_SECS: u64 = 100;
pub const DEFAULT_WAIT_SECS: u64 = 90;
/// The most text one tool answer carries; the MCP server cuts anything longer.
pub const MAX_TOOL_TEXT: usize = 60_000;
/// `plan_get` with a plan, as the chief reads it (pretty-printed JSON).
pub const MAX_PLAN_VIEW_CHARS: usize = 24_000;
/// One `wait` answer; later events follow from its cursor.
pub const MAX_WAIT_CHARS: usize = 32_000;
/// The hunks `diff` with `patch` shows, after its file list.
pub const MAX_PATCH_CHARS: usize = 40_000;
/// Files the diff summary lists by name.
pub const MAX_DIFF_FILES: usize = 300;
/// Terminal lines `read` shows.
pub const MAX_READ_LINES: usize = 400;

// A wait answers inside the client's 120 s, and each bounded answer fits the text limit.
const _: () = assert!(MAX_WAIT_SECS < 120 && DEFAULT_WAIT_SECS <= MAX_WAIT_SECS);
const _: () =
    assert!(MAX_PLAN_VIEW_CHARS < MAX_TOOL_TEXT && MAX_WAIT_CHARS < MAX_TOOL_TEXT && MAX_PATCH_CHARS < MAX_TOOL_TEXT);

/// `text` cut to `max` characters, saying how many more there were.
pub fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_string(),
        Some((cut, _)) => format!("{}… [{} more characters]", &text[..cut], text[cut..].chars().count()),
    }
}

/// The last `max` characters of `text`, saying how many came before.
pub fn clip_start(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let (cut, _) = text.char_indices().nth(count - max).expect("count > max");
    format!("[{} earlier characters] …{}", count - max, &text[cut..])
}

/// The first `max_items` of `items`, each clipped to `max` characters, and how many more there were.
pub fn clip_list(items: &[String], max: usize, max_items: usize) -> Vec<String> {
    let mut shown: Vec<_> = items.iter().take(max_items).map(|item| clip(item, max)).collect();
    if items.len() > max_items {
        shown.push(format!("[{} more]", items.len() - max_items));
    }
    shown
}

/// Every string in `value` clipped to `max` characters, and every array to its first `max_items`.
pub fn clip_json(value: &mut Value, max: usize, max_items: usize) {
    match value {
        Value::String(text) if text.chars().nth(max).is_some() => *text = clip(text, max),
        Value::Array(items) => {
            let more = items.len().saturating_sub(max_items);
            items.truncate(max_items);
            items.iter_mut().for_each(|item| clip_json(item, max, max_items));
            if more > 0 {
                items.push(Value::String(format!("[{more} more]")));
            }
        }
        Value::Object(map) => map.values_mut().for_each(|item| clip_json(item, max, max_items)),
        _ => {}
    }
}

/// How long `value` reads once pretty-printed, as the MCP server shows it.
pub fn shown_len(value: &Value) -> usize {
    serde_json::to_string_pretty(value).map_or(0, |text| text.len())
}

/// A plain-text rendering of a worktree diff for agents: a file summary (at most
/// [`MAX_DIFF_FILES`] names), and with `patch` the hunks, up to [`MAX_PATCH_CHARS`]. `path`
/// keeps only that file or the files under that folder, so the rest of a cut patch can be read.
pub fn describe_diff(diff: &WorktreeDiff, patch: bool, path: Option<&str>) -> String {
    let short = &diff.base[..diff.base.len().min(10)];
    let prefix = path.map(|path| path.trim_end_matches('/')).filter(|path| !path.is_empty());
    let files: Vec<_> = diff
        .files
        .iter()
        .filter(|file| {
            prefix.is_none_or(|prefix| {
                file.path() == prefix || file.path().strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/'))
            })
        })
        .collect();
    let mut out = format!(
        "{} files changed, +{} -{} against {} (merge-base {short})\n",
        diff.files.len(),
        diff.additions(),
        diff.deletions(),
        diff.base_ref
    );
    if let Some(prefix) = prefix {
        out.push_str(&format!("{} of them under {prefix}\n", files.len()));
    }
    for file in files.iter().take(MAX_DIFF_FILES) {
        let status = match file.status {
            FileStatus::Added => "added",
            FileStatus::Untracked => "untracked",
            FileStatus::Deleted => "deleted",
            FileStatus::Renamed => "renamed",
            FileStatus::Modified => "modified",
        };
        out.push_str(&format!("  {} (+{} -{}, {status})\n", file.path(), file.additions(), file.deletions()));
    }
    if files.len() > MAX_DIFF_FILES {
        out.push_str(&format!("  … {} more files; pass path (a folder) to list them\n", files.len() - MAX_DIFF_FILES));
    }
    if !patch {
        return out;
    }
    let mut shown = 0;
    for (position, file) in files.iter().enumerate() {
        let mut text = format!("\n--- {}\n", file.path());
        if file.binary {
            text.push_str("(binary)\n");
        }
        for hunk in &file.hunks {
            text.push_str(&hunk.header);
            text.push('\n');
            for line in &hunk.lines {
                let sign = match line.kind {
                    LineKind::Added => '+',
                    LineKind::Removed => '-',
                    LineKind::Context => ' ',
                    LineKind::Note => '\\',
                };
                text.push(sign);
                text.push_str(&line.text);
                text.push('\n');
            }
        }
        let size = text.chars().count();
        if shown + size > MAX_PATCH_CHARS {
            // Only a first file too long to show whole is shown in part; the rest are named.
            let partial = shown == 0;
            if partial {
                out.push_str(&clip(&text, MAX_PATCH_CHARS));
            }
            out.push_str(&format!("\n… patch cut at {MAX_PATCH_CHARS} characters"));
            if partial {
                out.push_str(&format!(", inside {}", file.path()));
            }
            out.push('.');
            let rest: Vec<_> = files[position + usize::from(partial)..].iter().map(|file| file.path()).collect();
            if !rest.is_empty() {
                let more = rest.len().saturating_sub(20);
                out.push_str(&format!(
                    " Not shown: {}{}. Pass path to read one file's hunks.",
                    rest[..rest.len() - more].join(", "),
                    if more > 0 { format!(" and {more} more") } else { String::new() }
                ));
            }
            out.push('\n');
            break;
        }
        shown += size;
        out.push_str(&text);
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
        let text = describe_diff(&diff, true, None);
        assert!(text.starts_with("1 files changed, +1 -0 against main (merge-base 0123456789)"));
        assert!(text.contains("  a.py (+1 -0, untracked)"));
        assert!(text.contains("+print(1)"));
    }

    fn large_diff(files: usize, lines: usize) -> WorktreeDiff {
        let content = format!("{}\n", "let value = compute(input, options);").repeat(lines);
        WorktreeDiff {
            base_ref: "main".into(),
            base: "0123456789abcdef".into(),
            files: (0..files)
                .map(|i| FileDiff::untracked(format!("src/f{i:03}.rs"), &content))
                .chain([FileDiff::untracked("srcs/other.rs".into(), "x\n")])
                .collect(),
            partial: false,
        }
    }

    #[test]
    fn a_large_diff_is_cut_to_fit_and_names_what_it_left_out() {
        let diff = large_diff(400, 200);
        let summary = describe_diff(&diff, false, None);
        assert!(summary.starts_with("401 files changed"));
        assert_eq!(summary.lines().filter(|line| line.starts_with("  src")).count(), MAX_DIFF_FILES);
        assert!(summary.contains("  … 101 more files; pass path (a folder) to list them"), "{summary}");

        let patch = describe_diff(&diff, true, None);
        let hunks = &patch[summary.len()..];
        let (shown, note) = hunks.rsplit_once("\n… patch cut at ").unwrap();
        assert!(shown.chars().count() <= MAX_PATCH_CHARS, "{}", shown.chars().count());
        assert!(shown.contains("--- src/f000.rs") && !shown.contains("more characters]"), "whole files only");
        assert!(note.contains("Not shown: src/f00") && note.contains("more. Pass path to read one file's hunks."));
        assert!(patch.len() < MAX_TOOL_TEXT, "{}", patch.len());

        // path reads one file, or a folder's files, and not a folder that only shares its prefix.
        let one = describe_diff(&diff, true, Some("src/f399.rs"));
        assert!(one.contains("1 of them under src/f399.rs") && one.contains("--- src/f399.rs"));
        assert!(!one.contains("patch cut"), "{}", &one[..200]);
        let folder = describe_diff(&diff, false, Some("src/"));
        assert!(folder.contains("400 of them under src") && !folder.contains("srcs/other.rs"));

        // A first file too long to show whole is shown in part.
        let huge = describe_diff(&large_diff(2, 5000), true, None);
        assert!(
            huge.contains(", inside src/f000.rs. Not shown: src/f001.rs, srcs/other.rs."),
            "{}",
            &huge[huge.len() - 300..]
        );
        assert!(huge.len() < MAX_TOOL_TEXT, "{}", huge.len());
    }

    #[test]
    fn clips_say_how_much_they_left_out() {
        assert_eq!(clip("héllo wörld", 5), "héllo… [6 more characters]");
        assert_eq!(clip("short", 5), "short");
        assert_eq!(clip_start("héllo wörld", 5), "[6 earlier characters] …wörld");
        assert_eq!(clip_start("short", 5), "short");
        let mut value = serde_json::json!({"input": {"content": "abcdef", "lines": [1, 2, 3, 4]}, "n": 7});
        clip_json(&mut value, 3, 2);
        assert_eq!(
            value,
            serde_json::json!({"input": {"content": "abc… [3 more characters]", "lines": [1, 2, "[2 more]"]}, "n": 7})
        );
    }
}
