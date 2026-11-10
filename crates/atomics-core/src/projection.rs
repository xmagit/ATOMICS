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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportInfo {
    pub status: String,
    pub summary: String,
    pub at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskInfo {
    pub id: String,
    pub title: String,
    pub status: TaskStatus,
    pub agent: String,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanInfo {
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

/// What the host says about itself beside the board: its workspace, ticket sign-in, account usage
/// and governor load. It never holds a token or a client secret; while a sign-in waits it holds the address to
/// open in a browser, as the desktop's banner shows it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostInfo {
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
            partial: diff.partial,
            files: diff
                .files
                .iter()
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
