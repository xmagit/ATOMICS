//! Read-only projections of the host's model, published as immutable snapshots.
//!
//! The runtime thread builds a projection from plain data and publishes it; any other thread loads
//! the current `Arc` and encodes or sends it with no `State` lock held. A collection's revision
//! moves only when its content does, so a reader can tell "nothing changed" by comparing numbers.
//! Revisions and the cursor are opaque strings on the wire (`wire::Cursor`); here they are `u64`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};

use crate::diff::{FileDiff, FileStatus, WorktreeDiff};
use crate::plan::{PlanStatus, TaskStatus};
use crate::session::AgentStatus;
use crate::steer::ReviewComment;
use crate::wire::Disposition;

/// One published value and the revision it was published at. Never mutated after publication.
#[derive(Debug, PartialEq, Eq)]
pub struct Snapshot<T> {
    pub revision: u64,
    pub data: T,
}

/// A slot holding the latest snapshot. `load` takes a lock only to clone an `Arc`; nothing
/// is computed or sent under it.
#[derive(Debug)]
pub struct Published<T> {
    slot: RwLock<Arc<Snapshot<T>>>,
}

impl<T: PartialEq + Default> Default for Published<T> {
    fn default() -> Self {
        Self { slot: RwLock::new(Arc::new(Snapshot { revision: 0, data: T::default() })) }
    }
}

impl<T: PartialEq> Published<T> {
    pub fn load(&self) -> Arc<Snapshot<T>> {
        self.slot.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Publish `data` unless it equals the current snapshot. Returns whether the revision moved.
    pub fn publish(&self, data: T) -> bool {
        let mut slot = self.slot.write().unwrap_or_else(|e| e.into_inner());
        if slot.data == data {
            return false;
        }
        *slot = Arc::new(Snapshot { revision: slot.revision + 1, data });
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskLink {
    pub plan: String,
    pub task: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Why the session waits for the user, when its ordinary status is insufficient.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub previews: Vec<crate::preview::Preview>,
    pub id: String,
    pub agent: String,
    pub title: String,
    /// A host path, never a browser path.
    pub cwd: String,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub status: AgentStatus,
    pub chief: bool,
    pub task: Option<TaskLink>,
    pub pending_approvals: u32,
    /// What its agent did in its own threads; absent on older hosts and before the first tool call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<ActivityInfo>,
}

/// The most threads a session's [`ActivityInfo`] keeps, and the longest id, kind or tool name in it.
pub const MAX_THREADS: usize = 8;
pub const MAX_THREAD_TEXT: usize = 64;

/// What a session's agent has done, from its hooks: the tool calls its own thread finished, and the subagents it
/// started with theirs. Names and counts only: never a tool's input or output.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityInfo {
    pub steps: u32,
    /// Oldest first; at most [`MAX_THREADS`], the oldest finished ones giving way.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub threads: Vec<ThreadInfo>,
    /// Recent tools on the main thread, newest first. The wire representation takes no tool arguments or output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_tools: Vec<ActivityStep>,
    /// What its own status line says while it works, read off its screen (`status_line`); absent when none shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<crate::status_line::LiveLine>,
    /// While it works: when its turn began, in seconds since the epoch, for a client to count from where its status
    /// line says nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
}

/// The most steps [`ActivityInfo::recent_tools`] keeps.
pub const MAX_RECENT: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityStep {
    pub tool: String,
    pub kind: ActivityKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    Read,
    Edit,
    Run,
    Search,
    Delegate,
    Plan,
    Communicate,
    Wait,
    #[serde(other)]
    Other,
}

impl ActivityStep {
    pub fn from_tool(tool: &str) -> Self {
        use ActivityKind::*;
        let kind = match tool {
            "Read" | "WebFetch" | "mcp__atomics__atomics_plan_get" => Read,
            "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => Edit,
            "Bash" | "exec_command" => Run,
            "Glob" | "Grep" | "WebSearch" => Search,
            "Agent" | "Task" | "mcp__atomics__atomics_plan_dispatch" => Delegate,
            "TodoWrite" | "mcp__atomics__atomics_plan_set" | "mcp__atomics__atomics_plan_update" => Plan,
            "mcp__atomics__atomics_send" | "mcp__atomics__atomics_report" => Communicate,
            "mcp__atomics__atomics_wait" => Wait,
            _ => Other,
        };
        Self { tool: short(tool), kind }
    }
}

/// A subagent: one of the threads an agent spawned.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadInfo {
    pub id: String,
    /// What kind of agent it is, as the agent names it (`Explore`, `general-purpose`, `default`).
    pub kind: String,
    pub running: bool,
    pub steps: u32,
    /// The last tool it called, by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
}

fn short(text: &str) -> String {
    let text: String = text.chars().filter(|c| !c.is_control()).collect();
    match text.char_indices().nth(MAX_THREAD_TEXT) {
        Some((at, _)) => text[..at].to_string(),
        None => text,
    }
}

impl ActivityInfo {
    /// Takes in one of the session's hook events.
    pub fn observe(&mut self, event: &crate::hook::HookEvent) {
        let finished = matches!(event.name.as_str(), "PostToolUse" | "PostToolUseFailure");
        let Some(id) = event.agent_id.as_deref().filter(|_| event.thread == crate::hook::HookThread::Spawned) else {
            if event.thread != crate::hook::HookThread::Main {
                return;
            }
            if finished {
                self.steps = self.steps.saturating_add(1);
            }
            if event.name == "PreToolUse"
                && let Some(tool) = event.tool_name.as_deref()
            {
                self.recent_tools.insert(0, ActivityStep::from_tool(tool));
                self.recent_tools.truncate(MAX_RECENT);
            }
            // A turn that ends has no subagent left working in it.
            if matches!(event.name.as_str(), "Stop" | "SessionEnd" | "Interrupt") {
                self.threads.iter_mut().for_each(|thread| thread.running = false);
            }
            return;
        };
        let id = short(id);
        let index = match self.threads.iter().position(|thread| thread.id == id) {
            Some(index) => index,
            None => {
                if event.name == "SubagentStop" {
                    return;
                }
                if self.threads.len() >= MAX_THREADS {
                    let oldest = self.threads.iter().position(|thread| !thread.running).unwrap_or(0);
                    self.threads.remove(oldest);
                }
                self.threads.push(ThreadInfo { id, running: true, ..Default::default() });
                self.threads.len() - 1
            }
        };
        let thread = &mut self.threads[index];
        if let Some(kind) = event.agent_type.as_deref().filter(|kind| !kind.is_empty()) {
            thread.kind = short(kind);
        }
        match event.name.as_str() {
            "SubagentStop" => thread.running = false,
            "SubagentStart" => thread.running = true,
            _ => {
                if let Some(tool) = event.tool_name.as_deref() {
                    thread.tool = Some(short(tool));
                }
                if finished {
                    thread.steps = thread.steps.saturating_add(1);
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportInfo {
    pub status: String,
    pub summary: String,
    pub at: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<crate::plan::Check>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<crate::plan::PullRequest>,
    pub id: String,
    pub title: String,
    pub status: TaskStatus,
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_reason: Option<String>,
    pub depends_on: Vec<String>,
    pub session: Option<String>,
    pub branch: Option<String>,
    pub integrated: Option<String>,
    pub additions: usize,
    pub deletions: usize,
    pub report: Option<ReportInfo>,
    /// The key of the ticket saying what the task is for (its id where it has no key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    /// Its definition of done, a check per entry, as the plan holds it; absent on older hosts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance: Vec<String>,
    /// The agent and model it runs, such as "Claude Opus 5.5" (`tickets::model_label`); absent on older hosts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// What the task asks, cleaned of credentials and cut to [`MAX_BRIEF`] bytes, for an open plan's tasks only;
    /// absent on older hosts.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub brief: String,
    /// The brief was cut to fit.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub brief_cut: bool,
}

/// The longest brief a task's projection carries, in UTF-8 bytes.
pub const MAX_BRIEF: usize = 2048;

/// Clean the entire open brief before bounding it so a secret spanning the limit is masked whole.
pub fn bounded_brief(brief: &str, cleaner: &crate::tickets::Cleaner) -> (String, bool) {
    let brief = cleaner.redacted(brief);
    if brief.len() <= MAX_BRIEF {
        return (brief, false);
    }
    let mut end = MAX_BRIEF;
    while !brief.is_char_boundary(end) {
        end -= 1;
    }
    (brief[..end].to_string(), true)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanInfo {
    #[serde(default)]
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<crate::plan::PullRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish: Option<crate::plan::Publish>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notebook: Vec<crate::plan::NotebookEntry>,
    #[serde(default)]
    pub coordination_enabled: bool,
    #[serde(default)]
    pub coordination: crate::notebook::Coordination,
    #[serde(default)]
    pub rules: crate::notebook::Rules,
    /// Full safe links for the shared ticket sidebar, chunked with their plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket_tree: Option<crate::ticket_ui::TicketTree>,
    pub id: String,
    pub status: PlanStatus,
    pub goal: String,
    pub project: Option<String>,
    pub approved: bool,
    pub integration: Option<String>,
    pub tasks: Vec<TaskInfo>,
    /// The digest of the plan's approval scope (goal and planned tasks), as the remote
    /// conversation's `/approve` uses it. `plan_command` `approve` and `set` name it, so the
    /// user approves or replaces exactly the plan they saw. Empty for a closed plan.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub digest: String,
    /// The key of the ticket (ticket mode) or board (board mode) the plan follows, its id where it has
    /// no key. A client opens the plan a ticket names by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChiefSelection {
    pub agent: crate::config::ChiefAgent,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChiefAuth {
    #[default]
    Unknown,
    SignedIn,
    SignedOut,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChiefProvider {
    pub agent: crate::config::ChiefAgent,
    pub installed: bool,
    pub auth: ChiefAuth,
    pub models: Vec<String>,
    pub quota: crate::usage::Usage,
    pub unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AvailableChiefs {
    pub selected: ChiefSelection,
    pub providers: Vec<ChiefProvider>,
    pub needs_confirmation: bool,
    pub switching: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChiefConversation {
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    pub agent: crate::config::ChiefAgent,
    pub conversation: String,
    pub turns: Vec<ChiefTurn>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChiefTurn {
    pub speaker: String,
    pub text: String,
}

/// What the host says about itself beside the board: its workspace, ticket sign-in, account usage
/// and governor load. It never holds a token or a client secret; while a sign-in waits it holds the address to
/// open in a browser, as the desktop's banner shows it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_chiefs: Option<AvailableChiefs>,
    /// A named placeholder when the host's auxiliary data cannot fit a frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
    /// Safe ticket reader data; absent on hosts without a ticket profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tickets: Option<crate::ticket_ui::TicketsInfo>,
    /// Editors run on the host; clients use these IDs with the terminal protocol.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub editors: Vec<EditorInfo>,
    /// Pull requests discovered by the host's GitHub/Azure workers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pull_requests: Vec<PullRequestInfo>,
    /// Open projects and their worktrees on the host. File trees are subscribed to separately.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<ProjectInfo>,
    /// The workspace open on the host, by name; `None` when none is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// The saved workspaces, by name, sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<String>,
    /// The configured ticket system's sign-in, when it signs in with OAuth from ATOMICS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sign_in: Option<SignInInfo>,
    /// Settings and host folders, absent on hosts predating the management UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub management: Option<ManagementInfo>,
    /// Account limits and machine load, sampled by the host, absent on older hosts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceInfo>,
    /// The host's version and what its update check found, absent on older hosts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<UpdateInfo>,
    /// Where the host's agents run, absent on older hosts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<AgentsInfo>,
}

/// Where a host's agents run, for a client to say in a word: the machine, how they are isolated by default and,
/// in ATOMICS's VM, whether its hardened mode is active.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentsInfo {
    pub machine: AgentMachine,
    /// The machine's name, where it has one worth showing: the VM's or the other machine's host name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// How an agent runs unless its task or agent entry says otherwise: `sandbox` or `bare`.
    pub isolation: String,
    /// In ATOMICS's VM: its system was built hardened and its user cannot become root. `None` elsewhere.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hardened: Option<bool>,
}

/// The machine a host's agents run on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMachine {
    /// The Mac the ATOMICS app runs on.
    #[default]
    Mac,
    /// ATOMICS's NixOS VM on a Mac.
    Vm,
    /// Another machine: a Linux host of its own.
    Other,
}

/// The host's version and any newer one. Every client reads it; only a window on the host can start an update,
/// and atomics.v1 has no operation for one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateInfo {
    /// The version running on the host.
    pub current: String,
    /// A newer version the last check found.
    pub latest: Option<String>,
    /// What is new, where the version alone does not say: the commits a checkout is behind.
    pub detail: Option<String>,
    /// The new version's release notes.
    pub notes: Option<String>,
    /// Why the last check failed; only Settings shows it.
    pub check_error: Option<String>,
    /// Why this install cannot take `latest`.
    pub unavailable: Option<String>,
    pub phase: UpdatePhase,
}

impl UpdateInfo {
    /// Whether the host has anything to say about updates beyond its version.
    pub fn has_news(&self) -> bool {
        self.latest.is_some() || self.check_error.is_some() || self.phase != UpdatePhase::Idle
    }
}

/// What an update is doing on the host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum UpdatePhase {
    #[default]
    Idle,
    /// Downloading, verifying or building: the step, in the user's words.
    Working { step: String },
    /// Installed; the host restarts once nothing below is in the way. It never forces that.
    WaitingForAgents { blockers: Vec<String> },
    /// Nothing known is in the way, and the window closes in this many seconds unless someone cancels. The host cannot
    /// see everything, a half-typed prompt for one.
    RestartsIn { seconds: u64 },
    /// Installed, the wait was cancelled, and this host still runs the old version.
    Installed { version: String },
    /// The update stopped, and why. A release leaves the install as it was; a checkout's install script may have put
    /// the new program in place before it stopped, and then says so.
    Failed { error: String },
    /// A phase from a newer host.
    #[serde(other)]
    Unknown,
}

/// Plain host data for the shared USAGE section and load gauge. The host's clock determines expiry
/// and source age, so clients on other machines need neither a native clock nor local pollers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResourceInfo {
    pub claude: crate::usage::Usage,
    pub codex: crate::usage::Usage,
    pub sampled_at: i64,
    pub load: Option<f64>,
    pub cores: usize,
    pub compiler_jobs: Option<usize>,
    pub test: SlotInfo,
    pub qa: SlotInfo,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disk: Vec<crate::machine::DiskWarning>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotInfo {
    pub slots: usize,
    pub running: usize,
    pub waiting: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorInfo {
    pub root: String,
    pub terminal: String,
    pub running: bool,
    pub file: Option<String>,
    pub inlay_hints: bool,
    /// Why the editor could not start, such as a program that is not installed on the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A PR's metadata. Its diff uses the usual paged `diff_get`, under `key`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestInfo {
    /// Additive metadata: older hosts omit these fields. Unknown membership stays unclassified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<crate::github::Group>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<bool>,
    pub key: String,
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub head: String,
    pub base: String,
    pub url: String,
    pub head_sha: Option<String>,
    pub loading: bool,
    pub error: Option<String>,
}

/// Editor IDs occupy a namespace separate from agent session IDs.
pub fn editor_terminal(root: &str) -> String {
    format!("editor:{root}")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub root: String,
    pub worktrees: Vec<WorktreeInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub path: String,
    pub branch: Option<String>,
}

/// Safe effective configuration and bounded folder listings computed by the host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagementInfo {
    pub config: serde_json::Value,
    pub proposals: Vec<ConfigProposalInfo>,
    pub folder_start: String,
    pub folders: Vec<FolderInfo>,
    pub recent: Vec<String>,
    pub remote: RemoteInfo,
    #[serde(default)]
    pub tools: serde_json::Value,
    #[serde(default)]
    pub themes: ThemesInfo,
    #[serde(default)]
    pub setup: SetupInfo,
}

/// What the setup screen shows, and the question a start that could not get a sandbox asks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SetupInfo {
    /// Local Mac onboarding. Absent on other hosts and on an existing Full installation.
    pub simple: Option<SimpleSetupInfo>,
    /// A first run that nobody has finished or skipped yet: the screen opens by itself.
    pub first_run: bool,
    /// The host is looking for the programs now.
    pub checking: bool,
    /// git and the agents, once the host has looked for them.
    pub programs: Vec<SetupProgram>,
    /// The first project open on the host.
    pub project: Option<String>,
    pub connections: Vec<SetupConnection>,
    pub start_choice: Option<StartChoice>,
    /// What sandboxes need on this machine, once the host has looked. It says what they would need while agents run
    /// on the machine too, so that someone can get ready before switching.
    pub sandbox_steps: Vec<SetupStep>,
    /// The step under way, one at a time.
    pub job: Option<SetupJob>,
    /// Why the last one did not finish. Its words hold no credential.
    pub failure: Option<SetupFailure>,
    /// The connections someone said will wait, by id.
    pub later: Vec<String>,
    /// This program runs from outside where it belongs, and can install itself there.
    pub install: Option<SetupInstall>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimpleSetupInfo {
    pub selected: bool,
    pub steps: Vec<SetupStep>,
}

/// A program that can install itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SetupInstall {
    /// Where it runs from, as the host shows a path.
    pub from: String,
    /// Where it would go.
    pub to: String,
}

/// What a click on a setup step does on the host. `atomics setup` runs the same work from a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupAction {
    Simple,
    Full,
    CheckVmRequirements,
    DownloadVm,
    CreateVm,
    VmLogin,
    CheckVmLogins,
    ConnectVm,
    SimpleMode,
    /// This program itself, from where it was downloaded to where it belongs.
    InstallSelf,
    InstallClaude,
    InstallCodex,
    InstallGit,
    InstallSbx,
    AzureLogin,
    DockerLogin,
    NetworkPolicy,
    /// Takes the value pasted beside it.
    ClaudeCredential,
    OpenAiLogin,
    /// Takes the value pasted beside it.
    OpenAiKey,
    LinearConnect,
    RemoveGithubSecret,
    SshForwardingOff,
    SshAgentOff,
}

/// A button on a setup step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupClick {
    pub action: SetupAction,
    pub label: String,
    /// It takes a pasted value, which never comes back from the host. This names the field.
    #[serde(default)]
    pub paste: Option<String>,
    /// It cannot be undone, so this is asked first.
    #[serde(default)]
    pub confirm: Option<String>,
}

impl SetupClick {
    pub fn new(action: SetupAction, label: &str) -> SetupClick {
        SetupClick { action, label: label.into(), paste: None, confirm: None }
    }
}

/// Something sandboxes need, and what can be done about it from here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SetupStep {
    pub name: String,
    pub done: bool,
    /// What it is, or what it still needs.
    pub note: String,
    pub clicks: Vec<SetupClick>,
    /// The exact command, where this takes a terminal or sudo.
    pub command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupJob {
    pub action: SetupAction,
    /// What it is doing now, for the person watching.
    pub progress: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupFailure {
    pub action: SetupAction,
    pub why: String,
}

/// A program setup looks for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SetupProgram {
    /// As a person calls it: Claude Code.
    pub name: String,
    /// The configured agent it runs; none for what no agent is, such as git.
    pub agent: Option<String>,
    /// Where it is, as the host shows a path.
    pub found: Option<String>,
    /// What to run to get it.
    pub install: String,
    /// The click that runs that, where no terminal or sudo is needed.
    pub click: Option<SetupClick>,
}

/// Something ATOMICS can connect to, which can always wait.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SetupConnection {
    /// `github`, `azure`, `tickets` or `remote`: says what its Connect button does.
    pub id: String,
    pub name: String,
    /// What it is connected as.
    pub connected: Option<String>,
    /// What connecting it means, or what it still needs.
    pub note: String,
    /// What to run in a terminal, where it takes one.
    pub command: Option<String>,
    /// The click that connects it, where one can.
    pub click: Option<SetupClick>,
}

/// A person's start that could not get a sandbox, waiting for them to say what to do. ATOMICS never runs a sandboxed
/// agent bare on its own, so this asks instead of failing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StartChoice {
    pub agent: String,
    /// What is missing, and what the sandbox's own tool said.
    pub why: String,
    /// Its repository requires sandboxed agents, so running it on this machine is no option.
    pub sandbox_required: bool,
}

/// What Settings offers to pick a theme from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemesInfo {
    /// The built-in themes, then the theme files.
    pub choices: Vec<ThemeChoice>,
    /// This host's own config file, as `config.d/<host>.toml`.
    pub host_file: String,
    /// That file names a theme, which wins over `config.toml`.
    pub host_picks: bool,
    /// The tokens `[theme]` sets inline, which show over any theme.
    pub inline: Vec<String>,
    /// What could not be applied of the theme in use.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeChoice {
    pub name: String,
    /// The theme in use.
    pub selected: bool,
    /// `themes/<name>.toml`, which may replace the built-in theme of that name.
    pub file: bool,
    /// As `#rrggbb`: background, panel, foreground, accent, then red, green and blue from its ANSI colors.
    pub swatches: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigProposalInfo {
    pub id: String,
    pub file: String,
    pub scope: String,
    pub from: String,
    pub reason: Option<String>,
    /// A credential-masked diff, never the raw file context.
    pub diff: String,
    pub risks: Vec<String>,
    pub warnings: Vec<String>,
    pub preview: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderInfo {
    pub requested: String,
    pub parent: Option<String>,
    pub project: bool,
    pub worktree: Option<String>,
    /// The same sorted, byte-bounded page used by the worktree explorer.
    pub page: crate::folder::Page,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteInfo {
    pub enabled: bool,
    pub status: String,
    pub problems: Vec<String>,
    pub pairing: Option<PairingInfo>,
    pub devices: Vec<DeviceInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PairingInfo {
    Idle {
        note: Option<String>,
    },
    Open {
        /// Only sent to a client granted host management; viewers receive no pairing secret.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        link: Option<String>,
        seconds_left: u64,
        busy: bool,
    },
    Confirm {
        name: String,
        code: String,
        seconds_left: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub controls: bool,
    #[serde(default)]
    pub host: bool,
    pub connected: bool,
    /// Unix milliseconds; older hosts omit it.
    #[serde(default)]
    pub paired_ms: u64,
    pub seen_ms: u64,
}

impl DeviceInfo {
    /// Paired and last seen dates in UTC, also usable by clients with no local clock or time zone.
    pub fn dates(&self) -> (String, String) {
        let date = |ms: u64| {
            if ms == 0 {
                return "unknown".to_string();
            }
            let Ok(at) = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000) else {
                return "unknown".to_string();
            };
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02} UTC",
                at.year(),
                u8::from(at.month()),
                at.day(),
                at.hour(),
                at.minute()
            )
        };
        (date(self.paired_ms), date(self.seen_ms))
    }
}

/// Where the ticket system's sign-in stands, as the Sign in and Reconnect buttons show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignInInfo {
    /// The ticket system, as `[tickets] system` names it.
    pub system: String,
    /// The server's host, never its path or query.
    pub server: String,
    pub state: SignInState,
    /// Why it needs a sign-in, or why the last one did not finish; free of tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// While a sign-in waits: the address to open in a browser, on any machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// While a sign-in waits: why the address pasted last was not used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected: Option<String>,
    /// While a sign-in waits: the host could not open its own browser, so the address has to be opened by hand.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub browser_failed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignInState {
    /// Signed in; the ticket tools work.
    SignedIn,
    /// Never signed in on the host, or signed out: "Sign in to <system>".
    SignedOut,
    /// It was signed in, but the server refused the saved sign-in or it ran out: "Reconnect".
    Expired,
    /// Signed in, but not for a resource the profile now reads from (its ticket API, say): one more
    /// sign-in covers it, and `reason` names it. What the sign-in covers keeps working.
    Partial,
    /// A sign-in is under way; `url` and `rejected` say how far.
    SigningIn,
    /// The host's keychain could not be read; `reason` says why.
    Unavailable,
    /// A state from a newer host.
    #[serde(other)]
    Unknown,
}

impl SignInInfo {
    /// Whether the ticket tools need the person: anything but signed in (or under way).
    pub fn needs_sign_in(&self) -> bool {
        matches!(self.state, SignInState::SignedOut | SignInState::Expired | SignInState::Partial)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalInfo {
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub input: serde_json::Value,
    pub id: String,
    pub session: String,
    pub tool: String,
    pub command: Option<String>,
    pub cwd: String,
    pub reason: Option<String>,
    /// What the host warns whoever decides about, such as whose shared stash entries the command drops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

impl ApprovalInfo {
    pub fn request(&self) -> crate::approvals::ApprovalRequest {
        crate::approvals::ApprovalRequest {
            tool: self.tool.clone(),
            command: self.command.clone(),
            cwd: self.cwd.clone().into(),
            reason: self.reason.clone(),
            input: self.input.clone(),
            warning: self.warning.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffFileSummary {
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub status: FileStatus,
    pub binary: bool,
    pub additions: usize,
    pub deletions: usize,
    /// Lines of an untracked file whose content was left out.
    pub omitted: usize,
}

pub const MAX_SUMMARY_FILES: usize = 128;

/// A worktree's diff without hunks: fetch a file's hunks with `diff_get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffSummary {
    pub worktree: String,
    /// The host's content revision for this worktree's diff.
    pub revision: u64,
    pub base_ref: String,
    pub base: String,
    pub partial: bool,
    pub files: Vec<DiffFileSummary>,
    /// Review comments on this worktree waiting to be sent to its agent, shared by every client.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<ReviewComment>,
}

impl DiffSummary {
    pub fn new(worktree: &str, revision: u64, diff: &WorktreeDiff) -> Self {
        Self {
            worktree: worktree.into(),
            revision,
            base_ref: diff.base_ref.clone(),
            base: diff.base.clone(),
            partial: diff.partial || diff.files.len() > MAX_SUMMARY_FILES,
            files: diff
                .files
                .iter()
                .take(MAX_SUMMARY_FILES)
                .map(|f| DiffFileSummary {
                    old_path: f.old_path.clone(),
                    new_path: f.new_path.clone(),
                    status: f.status,
                    binary: f.binary,
                    additions: f.additions(),
                    deletions: f.deletions(),
                    omitted: f.omitted,
                })
                .collect(),
            comments: Vec::new(),
        }
    }

    /// The event for a worktree whose diff is gone: no files and no base, at the last revision + 1.
    /// `wire` has no removal message for diffs, so this marks it.
    pub fn tombstone(worktree: &str, revision: u64) -> Self {
        Self {
            worktree: worktree.into(),
            revision,
            base_ref: String::new(),
            base: String::new(),
            partial: false,
            files: Vec::new(),
            comments: Vec::new(),
        }
    }

    pub fn is_tombstone(&self) -> bool {
        self.base.is_empty() && self.base_ref.is_empty() && self.files.is_empty()
    }

    pub fn additions(&self) -> usize {
        self.files.iter().map(|f| f.additions).sum()
    }

    pub fn deletions(&self) -> usize {
        self.files.iter().map(|f| f.deletions).sum()
    }
}

/// One file's hunks at a diff revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffFileBody {
    pub worktree: String,
    pub revision: u64,
    pub file: FileDiff,
}

/// All projections the protocol serves. Shared between the runtime thread (the only publisher)
/// and the protocol thread (a reader).
#[derive(Debug, Default)]
pub struct Projections {
    /// Machine samples have their own demand and do not advance the board cursor.
    pub machine: Published<crate::machine::Machine>,
    /// Host-only process roots used to resolve a preview against live kernel listeners.
    pub preview_roots: Published<Vec<(String, Option<u32>)>>,
    pub machine_watchers: Arc<std::sync::atomic::AtomicUsize>,
    pub sessions: Published<Vec<SessionInfo>>,
    pub plans: Published<Vec<PlanInfo>>,
    pub approvals: Published<Vec<ApprovalInfo>>,
    pub diffs: Published<Vec<DiffSummary>>,
    pub host: Published<HostInfo>,
    /// Bounded trace feed, outside the board cursor and replay ring.
    pub trace: Published<crate::trace_feed::TraceData>,
    cursor: AtomicU64,
    /// How approvals that left the board ended, newest last, so `approval_resolved` can say.
    resolutions: Mutex<VecDeque<(String, Disposition)>>,
}

/// Approval outcomes kept for `approval_resolved`; an older one is reported as withdrawn.
pub const MAX_RESOLUTIONS: usize = 256;

/// Everything at one moment, as sent in a `state_snapshot` and as the protocol thread reads it.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub cursor: u64,
    pub sessions: Arc<Snapshot<Vec<SessionInfo>>>,
    pub plans: Arc<Snapshot<Vec<PlanInfo>>>,
    pub approvals: Arc<Snapshot<Vec<ApprovalInfo>>>,
    pub diffs: Arc<Snapshot<Vec<DiffSummary>>>,
    pub host: Arc<Snapshot<HostInfo>>,
}

impl Projections {
    /// The event cursor: it moves by one for each collection that changed.
    pub fn cursor(&self) -> u64 {
        self.cursor.load(Ordering::Acquire)
    }

    fn moved(&self, changed: bool) -> bool {
        if changed {
            self.cursor.fetch_add(1, Ordering::AcqRel);
        }
        changed
    }

    pub fn publish_sessions(&self, data: Vec<SessionInfo>) -> bool {
        self.moved(self.sessions.publish(data))
    }

    pub fn publish_plans(&self, data: Vec<PlanInfo>) -> bool {
        self.moved(self.plans.publish(data))
    }

    pub fn publish_approvals(&self, data: Vec<ApprovalInfo>) -> bool {
        self.moved(self.approvals.publish(data))
    }

    pub fn publish_diffs(&self, data: Vec<DiffSummary>) -> bool {
        self.moved(self.diffs.publish(data))
    }

    pub fn publish_host(&self, data: HostInfo) -> bool {
        self.moved(self.host.publish(data))
    }

    /// Record how approvals ended (the newest `MAX_RESOLUTIONS`). Publish them before the
    /// approvals that no longer list them, so a reader that sees one gone finds how it ended.
    pub fn publish_resolutions(&self, resolved: &VecDeque<(String, Disposition)>) {
        let mut kept = self.resolutions.lock().unwrap_or_else(|e| e.into_inner());
        if *kept != *resolved {
            kept.clone_from(resolved);
            while kept.len() > MAX_RESOLUTIONS {
                kept.pop_front();
            }
        }
    }

    /// How the approval `id` ended, if it was answered lately.
    pub fn resolution(&self, id: &str) -> Option<Disposition> {
        let kept = self.resolutions.lock().unwrap_or_else(|e| e.into_inner());
        kept.iter().rev().find(|(i, _)| i == id).map(|(_, d)| *d)
    }

    /// Load every collection. The cursor is read first, so a publish racing the loads can only
    /// make the data newer than the cursor: the client then sees an event it already has, which
    /// replay treats as a no-op, never a gap.
    pub fn load(&self) -> Loaded {
        let cursor = self.cursor();
        Loaded {
            cursor,
            sessions: self.sessions.load(),
            plans: self.plans.load(),
            approvals: self.approvals.load(),
            diffs: self.diffs.load(),
            host: self.host.load(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(value: serde_json::Value) -> crate::hook::HookEvent {
        crate::hook::HookEvent::from_payload(&value).unwrap()
    }

    #[test]
    fn projected_activity_serializes_only_tool_names_counts_and_kinds() {
        let mut activity = ActivityInfo::default();
        for (name, input) in [
            ("Read", serde_json::json!({"file_path": "/project/private-acquisition.md"})),
            ("Bash", serde_json::json!({"command": "echo private-account-name"})),
            ("Grep", serde_json::json!({"pattern": "private-customer-name"})),
        ] {
            for event in ["PreToolUse", "PostToolUse"] {
                activity.observe(&hook(serde_json::json!({"hook_event_name": event, "session_id": "s",
                    "tool_name": name, "tool_input": input, "tool_response": "private-output"})));
            }
        }
        assert_eq!(
            serde_json::to_value(&activity).unwrap(),
            serde_json::json!({
                "steps":3, "recent_tools":[{"tool":"Grep","kind":"search"},{"tool":"Bash","kind":"run"},{"tool":"Read","kind":"read"}]
            })
        );
    }

    #[test]
    fn activity_counts_steps_and_follows_subagents_by_name_only() {
        let mut activity = ActivityInfo::default();
        let main = |name: &str| {
            hook(serde_json::json!({"hook_event_name": name, "session_id": "s",
            "transcript_path": "/t", "tool_name": "Edit", "tool_input": {"file_path": "/secret/path"}}))
        };
        let spawned = |name: &str, id: &str, tool: Option<&str>| {
            hook(serde_json::json!({"hook_event_name": name, "session_id": "s", "transcript_path": "/t",
                "agent_id": id, "agent_type": "Explore", "tool_name": tool, "tool_input": {"command": "rm -rf x"}}))
        };
        activity.observe(&main("PreToolUse"));
        activity.observe(&main("PostToolUse"));
        activity.observe(&main("PostToolUseFailure"));
        assert_eq!(activity.steps, 2, "finished calls of its own thread");
        assert!(activity.threads.is_empty());
        activity.observe(&spawned("SubagentStart", "a1", None));
        activity.observe(&spawned("PreToolUse", "a1", Some("Grep")));
        activity.observe(&spawned("PostToolUse", "a1", Some("Grep")));
        assert_eq!(activity.steps, 2, "a subagent's calls are its own");
        assert_eq!(
            activity.threads,
            [ThreadInfo {
                id: "a1".into(),
                kind: "Explore".into(),
                running: true,
                steps: 1,
                tool: Some("Grep".into())
            }]
        );
        let json = serde_json::to_string(&activity).unwrap();
        assert!(!json.contains("rm -rf") && !json.contains("/secret/path"), "names and counts only: {json}");
        activity.observe(&spawned("SubagentStop", "a1", None));
        assert!(!activity.threads[0].running);
        // A stop for a thread it never saw starts nothing.
        activity.observe(&spawned("SubagentStop", "ghost", None));
        assert_eq!(activity.threads.len(), 1);
        // Bounded: the oldest finished thread gives way first.
        for n in 0..MAX_THREADS + 3 {
            activity.observe(&spawned("SubagentStart", &format!("b{n}"), None));
        }
        assert_eq!(activity.threads.len(), MAX_THREADS);
        assert!(activity.threads.iter().all(|thread| thread.id != "a1"));
        // The main thread's turn ending leaves no subagent working.
        activity.observe(&main("Stop"));
        assert!(activity.threads.iter().all(|thread| !thread.running));
        // Long names are cut.
        activity.observe(&spawned("SubagentStart", &"x".repeat(500), None));
        let step = |tool: &str, input: serde_json::Value| {
            hook(serde_json::json!({"hook_event_name": "PreToolUse", "session_id": "s", "transcript_path": "/t",
                "tool_name": tool, "tool_input": input}))
        };
        for _ in 0..10 {
            activity.observe(&step("Grep", serde_json::json!({"pattern": "fn main"})));
        }
        assert_eq!(activity.recent_tools.len(), MAX_RECENT);
        assert!(activity.threads.iter().all(|thread| thread.id.chars().count() <= MAX_THREAD_TEXT));
    }

    #[test]
    fn the_countdown_reaches_a_client_and_an_older_one_reads_it_as_unknown() {
        let phase = UpdatePhase::RestartsIn { seconds: 7 };
        let wire = serde_json::to_value(&phase).unwrap();
        assert_eq!(wire, serde_json::json!({"state": "restarts_in", "seconds": 7}));
        assert_eq!(serde_json::from_value::<UpdatePhase>(wire).unwrap(), phase);
        let newer = serde_json::json!({"state": "something_later"});
        assert_eq!(serde_json::from_value::<UpdatePhase>(newer).unwrap(), UpdatePhase::Unknown);
    }

    fn session(id: &str, status: AgentStatus) -> SessionInfo {
        SessionInfo {
            status_hint: None,
            previews: Vec::new(),
            id: id.into(),
            agent: "claude".into(),
            title: id.into(),
            cwd: "/w".into(),
            worktree: None,
            branch: None,
            status,
            chief: false,
            task: None,
            pending_approvals: 0,
            activity: None,
        }
    }

    #[test]
    fn revision_moves_only_when_content_changes() {
        let p = Projections::default();
        assert_eq!(p.sessions.load().revision, 0);
        assert!(!p.publish_sessions(vec![]), "publishing the empty default is not a change");
        assert!(p.publish_sessions(vec![session("a", AgentStatus::Idle)]));
        assert!(!p.publish_sessions(vec![session("a", AgentStatus::Idle)]));
        assert!(p.publish_sessions(vec![session("a", AgentStatus::Working)]));
        assert_eq!(p.sessions.load().revision, 2);
        assert_eq!(p.cursor(), 2);
        assert!(p.publish_approvals(vec![ApprovalInfo {
            input: serde_json::Value::Null,
            id: "x".into(),
            session: "a".into(),
            tool: "Bash".into(),
            command: None,
            cwd: "/w".into(),
            reason: None,
            warning: None,
        }]));
        assert_eq!((p.approvals.load().revision, p.cursor()), (1, 3));
    }

    #[test]
    fn a_loaded_snapshot_is_immutable_across_later_publishes() {
        let p = Projections::default();
        p.publish_sessions(vec![session("a", AgentStatus::Idle)]);
        let held = p.load();
        p.publish_sessions(vec![]);
        assert_eq!(held.sessions.data.len(), 1);
        assert_eq!((held.sessions.revision, held.cursor), (1, 1));
        assert!(p.load().sessions.data.is_empty());
    }

    #[test]
    fn readers_proceed_while_another_thread_publishes() {
        let p = Arc::new(Projections::default());
        let writer = {
            let p = p.clone();
            std::thread::spawn(move || {
                for i in 0..200 {
                    p.publish_sessions(vec![
                        session("a", AgentStatus::Idle),
                        session(&i.to_string(), AgentStatus::Done),
                    ]);
                }
            })
        };
        let mut last = 0;
        while !writer.is_finished() {
            let s = p.sessions.load();
            assert!(s.revision >= last, "revisions never go backwards");
            last = s.revision;
        }
        writer.join().unwrap();
        assert_eq!(p.sessions.load().revision, 200);
    }
}

#[cfg(test)]
mod pull_request_tests {
    #[test]
    fn old_pull_request_metadata_is_still_readable() {
        let pull: super::PullRequestInfo = serde_json::from_value(serde_json::json!({
            "key": "pull:contoso/repo#1", "repo": "contoso/repo", "number": 1,
            "title": "Update parser", "head": "feature", "base": "main",
            "url": "https://example.com/pull/1", "head_sha": null, "loading": false, "error": null
        }))
        .unwrap();
        assert_eq!((pull.group, pull.author, pull.draft), (None, None, None));
    }
}
