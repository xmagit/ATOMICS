use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::sync::mpsc::channel;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use atomics_core::config::{AgentKind, ChiefDock, Config, VoiceTarget};
use atomics_core::git::{self, Integration, Merge, Worktree, WorktreeEntry};
use atomics_core::layout::{self, DiffPlacement, DiffSplit, Layout};
use atomics_core::session::{AgentStatus, UNTITLED, prompt_title, title_for};
use atomics_core::store::{SessionRecord, Store};
use atomics_core::{paths, steer, theme};
use egui::{Color32, Key, Pos2, Rect, RichText, Sense, Stroke, Vec2, pos2, vec2};

use crate::diff_view::{self, DiffState};
use crate::diff_worker::{DiffData, DiffWorker, WorkerResult};
use crate::governor::{Governor, Jobserver};
use crate::highlight::Highlighter;
use crate::ipc::{self, Channels, ControlMessage, ControlOutcome, HookMessage, Hub};
use crate::lifecycle::Scope;
use crate::sandbox::AgentRuntime;
use crate::setup::{SHELL_PATH, env_var, global_hooks_installed, install_global_hooks, resolve_program};
use crate::terminal::{Metrics, Terminal, TerminalView};
use crate::theme::{Palette, SystemFonts, Theme, shortcut, shortcut_label};
use crate::voice::{MAX_RECORDING, Recorder};
use crate::watcher::{Change, Watcher};
use atomics_core::approvals::{ApprovalRequest, Decision};
use atomics_core::control::{ControlRequest, ControlResponse};
use atomics_core::governor::{SlotClass, Ticket};
use atomics_core::hook::HookReply;
use atomics_core::plan::{self, Plan, PlanStatus, Report, Task, TaskStatus, short_slug};
use atomics_core::wire::{Disposition, ErrorCode, WireError};
use atomics_core::workspace::{self, Workspace};
#[cfg(test)]
use atomics_ui::chief::SteerTarget;
use atomics_ui::status::PULSE_STEP;
use atomics_ui::text::truncate;
use atomics_ui::{Backend, Command, PanelMode};
use serde_json::json;

mod agent_cleanup;
mod approvals;
mod backend;
mod background;
mod board;
mod chief;
mod commands;
mod connections;
mod control;
#[cfg(test)]
mod credentials;
mod editor;
mod fleet;
mod frame;
mod github;
mod heartbeat;
mod input;
mod integrations;
mod management;
mod mutations;
mod open_link;
mod plan_cleanup;
mod plan_read;
mod plan_view;
mod plans;
pub(crate) mod projection;
mod projects;
mod ref_guard;
mod remote;
mod repo_workflow;
mod restart;
mod sandbox_settings;
mod selfconfig;
mod sessions;
mod setup_screen;
mod sidebar;
mod themes;
mod tickets;
mod tickets_panel;
mod tickets_signin;
mod topbar;
mod trace;
mod trace_lane;
mod update;
mod voice;
mod workflow;
mod worktrees;

use chief::CHIEF_ALLOWED_TOOLS;
use editor::save_png;
use fleet::queue_note;

const WORKTREE_REFRESH: Duration = Duration::from_secs(30);
const BACKGROUND_REFRESH: Duration = Duration::from_secs(5);
const FALLBACK_REFRESH: Duration = Duration::from_secs(30);
/// The answer to a request only a desktop window can serve, in the headless host.
const NO_WINDOW: &str = "no local window: this is the headless host (atomics host); window control commands are not forwarded to connected clients; use the client's UI";
/// Untracked content kept for a diff off screen; the rest is counted, not loaded.
const OFF_SCREEN_UNTRACKED: u64 = 1024 * 1024;

#[derive(Default)]
pub struct Options {
    /// Projects to open, from `atomics <path>`; empty means the launch folder or the last session.
    pub projects: Vec<PathBuf>,
    pub workspace: Option<String>,
    pub open: Vec<(String, PathBuf)>,
    pub screenshot: Option<PathBuf>,
    pub screenshot_delay: f32,
    pub migration_errors: Vec<String>,
    /// An `atomics://open` link the launch was given, handled once the app has loaded.
    pub link: Option<crate::open_link::Incoming>,
    /// `atomics host --remote-port`: `[remote]` on, on this port, whatever the config file says.
    pub remote_port: Option<u16>,
}

pub struct Session {
    pub id: String,
    pub agent: String,
    pub kind: AgentKind,
    pub terminal: Option<Terminal>,
    pub claude_id: Option<String>,
    pub cwd: PathBuf,
    pub worktree: Option<Worktree>,
    worktree_ready: Option<Receiver<Option<Worktree>>>,
    pub title: String,
    pub status: AgentStatus,
    pub last_prompt: Option<String>,
    pub last_message: Option<String>,
    hooks_active: bool,
    activity: heartbeat::Activity,
    approvals: Vec<PendingApproval>,
    allowed: Vec<ApprovalRequest>,
    held: Vec<String>,
    approval_hold: bool,
    menu_absent_since: Option<Instant>,
    /// The command menu just answered with keys, and when: still on screen until the agent redraws,
    /// it is not asked again.
    answered_menu: Option<(atomics_core::approvals::NativeMenu, Instant)>,
    /// The chief agent stays outside the agent fleet.
    pub chief: bool,
    /// A message typed into the terminal that the agent has not visibly picked up yet.
    pending: Option<Pending>,
    /// The sandbox a sandboxed agent runs in.
    sandbox: Option<String>,
    /// Its placeholder for the Claude subscription, revoked when its agent exits or it closes.
    relay: Option<crate::relay::Granted>,
    screen_busy: bool,
    busy_absent_since: Option<Instant>,
    ref_guard: Option<crate::ref_guard::Shared>,
    /// The scratch folder this session works in, outside any project; it goes when the session closes.
    scratch: Option<PathBuf>,
    naming: Naming,
    /// MCP servers this sandboxed agent needs that are not connected: its tile offers to connect them.
    mcp_needed: Vec<connections::Needed>,
    /// Something about how its agent runs that its tile warns of, such as Codex started without hooks.
    warning: Option<String>,
}

/// Where a session's title comes from. Only a folder's title changes by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Naming {
    /// Its branch or folder name, following the session's folder.
    Folder,
    /// A scratch session waits for its first prompt to name it.
    FirstPrompt,
    /// Named by its first prompt or the user, and kept.
    Named,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    Sent,
    Pending,
    Queued,
    Held,
    Unavailable,
}

struct PendingApproval {
    id: String,
    request: ApprovalRequest,
    reply: Option<Arc<ipc::ApprovalReply>>,
    /// The command menu the agent drew with no hook asking; only its keys answer it, and only
    /// while the screen still shows it.
    native: Option<atomics_core::approvals::NativeMenu>,
    /// The stash entries the request's warning named, by commit; empty unless its command drops some.
    stash: approvals::StashPins,
}

/// Who a session is started as: the chief, or an agent with its plan task's own isolation, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Chief,
    Agent(Option<atomics_core::isolation::IsolationMode>),
}

struct AgentLaunch {
    id: String,
    agent: atomics_core::config::AgentConfig,
    cwd: PathBuf,
    resume: Option<String>,
    extra_args: Vec<String>,
    prompt: Option<String>,
    chief: bool,
    runtime: AgentRuntime,
    /// A sandboxed session's ref guard, opened and checked before its agent starts.
    ref_guard: Option<crate::ref_guard::Shared>,
    /// A sandboxed Claude's placeholder for the Claude subscription.
    relay: Option<crate::relay::Granted>,
    /// A person pressed Start for it.
    interactive: bool,
}

/// What preparing a sandbox gives its launch: the ref guard with what its first check found, the
/// session's relay placeholder and the MCP servers it needs that are not connected.
type Prepared = Result<(crate::ref_guard::Opened, Option<crate::relay::Granted>, Vec<String>), NotPrepared>;

/// Why a launch did not get as far as starting its agent.
#[derive(Debug)]
enum NotPrepared {
    /// No sandbox could be had for it: what a person who pressed Start gets asked about.
    Sandbox(String),
    Other(String),
}

impl From<NotPrepared> for String {
    fn from(failure: NotPrepared) -> String {
        match failure {
            NotPrepared::Sandbox(why) | NotPrepared::Other(why) => why,
        }
    }
}

struct Pending {
    text: String,
    sent: Instant,
    attempts: u8,
    was_working: bool,
    codex: bool,
    screen_before: String,
    paste_seen: bool,
    reply: Option<Sender<ControlOutcome>>,
}

type CodexVersion = Arc<OnceLock<Option<bool>>>;
/// Each repository with the worktrees git lists for it.
type ListedWorktrees = Vec<(PathBuf, Vec<WorktreeEntry>, atomics_core::identity::Identity)>;

const APPROVAL_CHECK_INTERVAL: Duration = Duration::from_millis(250);
const APPROVAL_MENU_GRACE: Duration = Duration::from_secs(1);
const NEEDS_INPUT_CHECK_INTERVAL: Duration = Duration::from_secs(30);

const DELIVERY_TIMEOUT: Duration = Duration::from_secs(4);
const DELIVERY_ATTEMPTS: u8 = 3;
const CODEX_VERSION_TIMEOUT: Duration = Duration::from_secs(30);
/// What a Codex tile says when its version probe failed, so it runs without ATOMICS's hooks.
const CODEX_HOOKS_UNKNOWN: &str = "Started without ATOMICS hooks: `codex --version` failed, so hook support is \
                                   unknown. Approvals stay in this terminal; ATOMICS checks again at the next start.";
/// How long a quit waits for sandboxes being prepared, and then again for those whose creation it
/// stopped to clean up.
const QUIT_WAIT: Duration = Duration::from_secs(10);
/// How long a quit waits for sandbox and worktree cleanup still running.
const QUIT_JOBS_WAIT: Duration = Duration::from_secs(60);

struct NewDialog {
    agent: String,
    place: Place,
    folder: String,
    repo: String,
    branch: String,
    /// The first prompt, which also names a scratch session.
    prompt: String,
    /// This session's own isolation; `None` follows the settings.
    isolation: Option<atomics_core::isolation::IsolationMode>,
    error: Option<String>,
}

/// Where a new session works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    /// A new folder of its own, outside any project.
    Scratch,
    Folder,
    Worktree,
}

/// A session being renamed and the title typed so far.
struct Rename {
    session: String,
    title: String,
    focus: bool,
}

/// An agent to start once its new worktree is checked out.
struct Launch {
    agent: String,
    prompt: Option<String>,
    args: Vec<String>,
    reply: Option<Sender<ControlOutcome>>,
    /// Plan task this agent works on.
    task: Option<(String, String)>,
    /// That task's own isolation.
    isolation: Option<atomics_core::isolation::IsolationMode>,
    /// A person pressed Start for it.
    interactive: bool,
}

/// A background worktree job completed all preparation needed before launch.
struct PreparedWorktree {
    path: PathBuf,
    /// The context applied on the serial identity worker, checked again before launch.
    identity: Option<atomics_core::identity::Identity>,
    warmed: bool,
    /// The commit the branch was created at, when this made it.
    start: Option<String>,
    /// Why it may start behind the remote: the fetch before it failed.
    warning: Option<String>,
}

/// A finished plan integration, to note on its task and answer.
struct Integrated {
    commit: Option<String>,
    plan: String,
    task: String,
    branch: String,
    into: String,
    result: Result<Integration, String>,
    reply: Option<Sender<ControlOutcome>>,
}

struct CheckedIntegration {
    /// Which check of the task this is: one that ran past its time limit no longer counts.
    id: u64,
    commit: Option<String>,
    revision: u64,
    plan: String,
    task: Task,
    into: String,
    /// `None` means git could not verify ancestry; keep the integration marker.
    valid: Option<bool>,
    /// Why git could not tell, or did not run or finish: shown on the task, and checked again after a
    /// while, not only when refs change.
    problem: Option<String>,
}

/// A dispatch held until the integration checks of the dependencies it waits on report.
struct DispatchWaiter {
    plan: String,
    only: Option<String>,
    /// `None` for the board's buttons, which toast an error instead.
    reply: Option<Sender<ControlOutcome>>,
    deadline: Instant,
}

/// The worktree clean-up dialog for one project.
struct Cleanup {
    repo: PathBuf,
    rows: Option<Vec<git::WorktreeHealth>>,
    sandboxes: crate::sandbox::RepoSandboxes,
    busy: Option<String>,
    log: Vec<String>,
    tx: crate::host::Sender<CleanupEvent>,
    rx: Receiver<CleanupEvent>,
}

enum CleanupEvent {
    Loaded(Result<Vec<git::WorktreeHealth>, String>),
    Sandboxes(Result<crate::sandbox::RepoSandboxes, String>),
    Removed(PathBuf, Result<(), String>),
    SandboxRemoved(String, Result<(), String>),
    Finished(String),
}

/// A status change, kept so `wait` can answer "what happened since cursor N".
struct StatusEvent {
    seq: u64,
    session: String,
    title: String,
    status: AgentStatus,
    at: u64,
    plan: Option<String>,
    task: Option<String>,
    /// Set for a `atomics_report` from the agent: (status, summary, task, agent).
    report: Option<(String, String, Option<String>, String)>,
    /// The pending request the event is about, with its id (`approval_json`).
    approval: Option<serde_json::Value>,
    findings: Option<Vec<atomics_core::heartbeat::Finding>>,
    /// Set when someone changed a linked ticket in its system.
    ticket: Option<serde_json::Value>,
    /// Set when the user applied or rejected a proposed config change.
    config: Option<serde_json::Value>,
    /// Set when a plan was approved, edited, closed or reopened.
    change: Option<&'static str>,
}

impl StatusEvent {
    /// What `wait` matches on: the agent's status, "reported", "heartbeat", "ticket", "config" or "plan".
    fn kind(&self) -> &str {
        if self.findings.is_some() {
            "heartbeat"
        } else if self.ticket.is_some() {
            "ticket"
        } else if self.config.is_some() {
            "config"
        } else if self.change.is_some() {
            "plan"
        } else if self.report.is_some() {
            "reported"
        } else {
            self.status.label()
        }
    }

    fn to_json(&self) -> serde_json::Value {
        let mut value = json!({ "seq": self.seq, "session": self.session, "title": self.title, "status": self.kind(), "at": self.at });
        value["plan"] = json!(self.plan);
        value["task"] = json!(self.task);
        if let Some((status, summary, task, agent)) = &self.report {
            value["report"] = json!({ "status": status, "summary": summary, "task": task,
                "data_notice": plan::REPORT_DATA_NOTICE, "provenance": plan::agent_provenance("report", task.as_deref(), agent, self.at) });
        }
        if let Some(findings) = &self.findings {
            value["findings"] = json!(findings);
        }
        if let Some(ticket) = &self.ticket {
            value["ticket"] = ticket.clone();
        }
        if let Some(config) = &self.config {
            value["config"] = config.clone();
        }
        if let Some(change) = self.change {
            value["change"] = json!(change);
        }
        if let Some(request) = &self.approval {
            value["approval"] = request.clone();
        }
        value
    }
}

/// A `wait` request held until a matching event or its deadline.
struct Waiter {
    sessions: Vec<String>,
    statuses: Vec<String>,
    after: u64,
    compact: bool,
    full_sessions: bool,
    deadline: Instant,
    reply: Sender<ControlOutcome>,
}

/// A wait answers well inside the MCP client's limit for one call.
const MAX_WAIT: Duration = Duration::from_secs(atomics_core::control::MAX_WAIT_SECS);

struct QuickOpen {
    query: String,
    selected: usize,
    focus: bool,
}

/// Push-to-talk: recording from the microphone, then waiting for the transcript.
enum Voice {
    Idle,
    Opening(Arc<AtomicBool>),
    Recording(Recorder),
    Transcribing,
}

enum VoiceEvent {
    Opened(Arc<AtomicBool>, Result<Recorder, String>),
    Transcript(Result<String, String>),
}

struct Screenshot {
    path: PathBuf,
    at: f64,
    requested: bool,
    done: Arc<AtomicBool>,
}

/// The host's native model and services. The desktop borrows the model to read
/// content and publish view hints; runtime effects run through its action queue.
/// Drafts, selections, dialogs, terminal painting and microphone capture are client-owned.
pub(crate) struct State {
    /// The desktop's window, or none in the headless host.
    window: crate::host::Window,
    /// The protocol server and rendezvous as the host started them, for the top bar and ctl.
    remote: crate::host::RemoteView,
    folder_cache: Vec<atomics_core::projection::FolderInfo>,
    management_config: std::sync::OnceLock<serde_json::Value>,
    credential_status: serde_json::Value,
    credential_busy: bool,
    /// `[remote]` as `atomics host --remote-port` set it, applied over every config load.
    remote_port: Option<u16>,
    wake: crate::host::Wake,
    control_tx: crate::host::Sender<ControlMessage>,
    client_commands: Vec<ControlRequest>,
    client_events: Vec<ClientEvent>,
    action_tx: crate::host::Sender<commands::Action>,
    action_rx: Receiver<commands::Action>,
    config: Config,
    config_path: PathBuf,
    config_stamp: [Option<u64>; 3],
    repo_workflows: HashMap<PathBuf, atomics_core::repo_workflow::RepoWorkflow>,
    /// The project each folder belongs to, asked of git once per folder.
    project_roots: HashMap<PathBuf, PathBuf>,
    config_checked: Instant,
    /// Why the config no longer loads; the last good one stays in effect.
    config_error: Option<String>,
    /// What the loaded config asks for that cannot work (a `[github]` entry, a shortcut clash);
    /// the rest of it applies.
    config_problems: Option<String>,
    /// What the theme files or `[theme]` could not apply; the rest of the theme still does.
    theme_error: Option<String>,
    /// This machine's config.d file picks a theme, which wins over config.toml's.
    host_theme: bool,
    /// What Settings offers to pick a theme from; read with `host_theme`.
    themes: atomics_core::projection::ThemesInfo,
    /// The theme as resolved: what terminals answer and the highlighter draws with. Each change is a new
    /// `Arc`, which tells clients to make their palette again.
    theme: Arc<Theme>,
    highlighter: Arc<Highlighter>,
    sessions: Vec<Session>,
    previews: crate::preview::Catalog,
    pub(crate) machine: crate::machine::Collector,
    codex_versions: HashMap<String, CodexVersion>,
    pending_codex: Vec<(AgentLaunch, CodexVersion)>,
    /// The `sbx` program sandboxed agents run with.
    sbx: String,
    sandbox_usage: crate::sandbox_usage::Sampler,
    /// The credential relay sandboxed Claude agents reach the Claude subscription through.
    relay: Arc<crate::relay::Service>,
    /// MCP connections: what sbx and the imported definitions say, and the work under way.
    connections: connections::Connections,
    /// The GitHub counter and controls: what the API last said, sign-in and actions under way.
    github: github::Github,
    lifecycle_jobs: Vec<crate::lifecycle::Job>,
    /// Closed plans' worktrees whose removal waits for the processes still using them; kept on disk.
    deferred_cleanup: Vec<crate::lifecycle::Deferred>,
    /// When they were last checked, and whether a check is under way.
    deferred_checked: Option<Instant>,
    deferred_checking: Arc<AtomicBool>,
    /// The plan whose cleanup is closing its sessions, and covers their worktrees itself.
    cleaning_plan: Option<String>,
    startup_cleanup: Option<std::thread::JoinHandle<()>>,
    shutting_down: bool,
    sandbox_check: Option<Receiver<(Vec<String>, Vec<atomics_core::projection::SetupStep>)>>,
    sandbox_lines: Vec<String>,
    sandbox_steps: Vec<atomics_core::projection::SetupStep>,
    programs_check: Option<Receiver<setup_screen::ProgramsChecked>>,
    setup_programs: Vec<atomics_core::projection::SetupProgram>,
    /// Azure's CLI, once it has been looked for.
    setup_azure: crate::relay::azure::Status,
    setup_azure_install: Option<String>,
    setup_job: Option<setup_screen::RunningSetup>,
    setup_failure: Option<atomics_core::projection::SetupFailure>,
    /// A step finished while a look was under way, whose answer may be from before it.
    setup_look_again: bool,
    /// This program runs from outside where it belongs: from there, and where it would go.
    install_offer: Option<(PathBuf, PathBuf)>,
    /// The folder ATOMICS started in is in a repository, so that its welcome starts agents there.
    launch_project: bool,
    /// A person's start that could not get a sandbox, until they say what to do.
    start_choice: Option<setup_screen::PendingStart>,
    /// The next launch comes from a person pressing Start, and so may ask them about it.
    interactive_start: bool,
    /// Sandboxed launches waiting for their sandbox, and their ref guard with what its first check found.
    pending_sandbox: Vec<(AgentLaunch, Receiver<Prepared>)>,
    ref_checks: crate::ref_guard::Checks,
    identity_updates: crate::ref_guard::Checks,
    active: Option<String>,
    diff_open: bool,
    /// Worktree picked in the sidebar without an agent; the diff otherwise follows the active session.
    diff_root: Option<PathBuf>,
    diffs: HashMap<PathBuf, DiffData>,
    /// Chief-only, reduced MCP history. Never contains raw tool responses.
    traces: HashMap<String, atomics_core::trace::TraceLog>,
    worker: DiffWorker,
    hub: Arc<Mutex<Hub>>,
    hook_rx: Receiver<HookMessage>,
    usage: crate::usage::UsagePoller,
    /// Account usage that sandboxed agents saw, read back from their VMs.
    sandbox_pull: crate::usage::SandboxPull,
    /// The ATOMICS VM on a Mac with its bundle installed, for the top bar (`vm.rs`).
    vm: Option<crate::vm::Monitor>,
    control_rx: Receiver<ControlMessage>,
    /// Mutations from protocol clients, carried out in turn with everything else (`mutations.rs`).
    op_tx: crate::host::Sender<crate::protocol::OpRequest>,
    op_rx: Receiver<crate::protocol::OpRequest>,
    /// The operation journal: in memory, or in a file while the protocol server runs.
    ops: crate::protocol::journal::Journal,
    in_flight: Vec<mutations::InFlight>,
    op_replies: Vec<(crate::protocol::OpReply, String, atomics_core::ops::Outcome)>,
    /// How approvals ended lately, newest last: for a late decision and `approval_resolved`.
    resolved: std::collections::VecDeque<(String, Disposition)>,
    hook_error: Option<String>,
    /// Machine-wide limits for the agents' heavy work.
    governor: Arc<Governor>,
    /// Compiler job pool every agent inherits; None when this instance does not own the socket.
    jobserver: Option<Arc<Jobserver>>,
    /// Account usage and governor state published to every client.
    resources: atomics_core::projection::ResourceInfo,
    resources_checked: Instant,
    /// What the update check found, and an update under way.
    update: update::Updates,
    busy_checked: Instant,
    output_delay: Option<Duration>,
    approvals_checked: Instant,
    needs_input_checked: Instant,
    store: Store,
    dismissed: HashSet<String>,
    /// Main checkouts of the open projects (plus `repos` from the config).
    repos: Vec<PathBuf>,
    open_projects: Vec<PathBuf>,
    workspace: Option<String>,
    /// The saved workspaces, by name, as last read: the host's state lists them for every client.
    workspace_names: Vec<String>,
    /// Worktree folder → its project's main checkout.
    worktree_project: HashMap<PathBuf, PathBuf>,
    git_identities: HashMap<PathBuf, atomics_core::identity::Identity>,
    title: String,
    /// Diff/editor width set by dragging the divider.
    /// Side by side or unified, picked in the diff header; wins over the config until ATOMICS quits.
    cleanup: Option<Cleanup>,
    plan_cleanup: Option<plan_cleanup::PlanCleanup>,
    panel: PanelMode,
    editors: HashMap<PathBuf, Terminal>,
    editor_files: HashMap<PathBuf, String>,
    events: Vec<StatusEvent>,
    event_seq: u64,
    last_status: HashMap<String, AgentStatus>,
    waiters: Vec<Waiter>,
    heartbeat: heartbeat::Heartbeat,
    agent_cleanup: agent_cleanup::Checks,
    /// Events already summarized in the last automatic chief wake.
    chief_wake_cursor: u64,
    tickets: tickets::Tickets,
    plans: Vec<Plan>,
    selected_plan: usize,
    changed_plans: HashSet<String>,
    /// The project tab on screen; `None` shows every project.
    view: Option<PathBuf>,
    plan_view: bool,
    /// The Ungrouped tab is on screen: every session outside a plan.
    ungrouped: bool,
    /// The chief is shown, either floating or docked (otherwise only the pill).
    chief_open: bool,
    /// `[chief] autostart` waits for the first background tick, after a restart's sessions resume.
    chief_autostart: bool,
    worktree_tx: crate::host::Sender<(String, Result<PreparedWorktree, String>, Launch, PathBuf)>,
    worktree_rx: Receiver<(String, Result<PreparedWorktree, String>, Launch, PathBuf)>,
    creating: Vec<(PathBuf, String, PathBuf)>,
    integrate_tx: crate::host::Sender<Integrated>,
    integrate_rx: Receiver<Integrated>,
    /// Tasks being merged into the integration branch.
    integrating: HashSet<(String, String)>,
    integration_check_tx: crate::host::Sender<CheckedIntegration>,
    integration_check_rx: Receiver<CheckedIntegration>,
    /// Readiness checks running, each with its id and the time it counts as failed by.
    integration_checks: HashMap<(String, String), (u64, Instant)>,
    integration_check_id: u64,
    integration_checked: HashMap<(String, String), (u64, String, Task)>,
    integration_retries: HashMap<(String, String), Instant>,
    /// Why the last check of a task could not tell whether it is integrated.
    integration_problems: HashMap<(String, String), String>,
    identity_context:
        Option<(atomics_core::identity::IdentityConfig, Option<atomics_core::identity::Identity>, Vec<PathBuf>)>,
    dispatch_waiters: Vec<DispatchWaiter>,
    integration_dirty: Arc<AtomicBool>,
    integration_revision: u64,
    worktrees: Vec<WorktreeEntry>,
    worktrees_at: Option<Instant>,
    /// The worktrees of each repository, being listed off the UI thread: git can be made to wait.
    worktree_listing: Option<Receiver<ListedWorktrees>>,
    focus_terminal: Option<String>,
    last_layout: Option<Layout>,
    watcher: Option<Watcher>,
    watch_tx: crate::host::Sender<PathBuf>,
    watch_rx: Receiver<PathBuf>,
    toast: Option<(String, Instant)>,
    /// A link the launch was given, or macOS delivered, opened once the ticket state is known.
    pending_link: Option<crate::open_link::Incoming>,
    /// When the pending link was first seen waiting; one waiting too long is dropped with a toast.
    pending_link_since: Option<Instant>,
    /// Text for the chief's prompt that is typed but never sent, once the chief is up.
    chief_draft: Option<String>,
    pending_open: Vec<(String, PathBuf)>,
    resume: Vec<SessionRecord>,
    restarting: bool,
    restart_event: Option<serde_json::Value>,
    restart_layout: Option<atomics_core::store::RestartState>,
    plan_edit_open: bool,
    pending_questions: HashSet<String>,
    /// Config changes proposed through the tools, waiting for the user, oldest first.
    proposals: Vec<selfconfig::Pending>,
    proposal_seq: u64,
    desktop_split: Option<f32>,
    desktop_diff_split: Option<DiffSplit>,
    launch_dir: PathBuf,
}

/// Native effects delivered to the desktop; these never drive runtime progress.
enum ClientEvent {
    PullRequest(String),
    ApprovalDecided {
        session: String,
        approval: String,
    },
    Cleanup {
        preselect: Option<PathBuf>,
    },
    PlanCleanup,
    PlanCleanupPreview(Vec<(PathBuf, String)>),
    OpenFile {
        root: PathBuf,
        path: String,
        whole_file: bool,
    },
    Edit {
        root: PathBuf,
        path: Option<String>,
    },
    Screenshot {
        path: String,
        reply: Sender<ControlOutcome>,
    },
    VoiceFailed {
        text: String,
        error: String,
    },
    ConfigChanged,
    Viewport(egui::ViewportCommand),
    /// Open or close the Settings menu, for `show_settings`.
    ShowSettings(bool),
}

/// View data belongs to the desktop, never to the runtime scheduler.
struct Client {
    /// The view state the draw code in atomics-ui keeps; the fields below are the desktop's own.
    ui: atomics_ui::Client,
    ctx: egui::Context,
    /// The host's theme in egui colors and fonts, with the theme it was made from.
    palette_cache: std::cell::RefCell<Option<(Arc<Theme>, Arc<Palette>)>>,
    project_picker: Option<PathBuf>,
    cleanup_open: bool,
    cleanup_selected: HashSet<PathBuf>,
    /// Orphaned sandboxes picked for removal in the clean-up dialog.
    cleanup_sandboxes: HashSet<String>,
    cleanup_delete_branches: bool,
    plan_cleanup_open: bool,
    plan_cleanup_selected: HashSet<(PathBuf, String)>,
    terminal_views: HashMap<egui::Id, TerminalView>,
    new_dialog: Option<NewDialog>,
    rename: Option<Rename>,
    quick_open: Option<QuickOpen>,
    voice: Voice,
    /// The GitHub modal's selection and confirmation, and the pull request diff's view state.
    github: github::GithubUi,
    /// Settings → Connections' fields for adding a remote server.
    #[cfg(test)]
    connection_name: String,
    #[cfg(test)]
    connection_url: String,
    /// Open or close the Settings menu on the next frame.
    show_settings: Option<bool>,
    /// Nothing had the keyboard when the last frame ended: an Escape now is no field's or terminal's.
    keyboard_idle: bool,
    voice_tx: Sender<VoiceEvent>,
    voice_rx: Receiver<VoiceEvent>,
    screenshot: Option<Screenshot>,
}

impl Client {
    /// `theme` in egui colors and fonts, made again only when the host's theme changed.
    fn palette(&self, theme: &Arc<Theme>) -> Arc<Palette> {
        let mut made = self.palette_cache.borrow_mut();
        match &*made {
            Some((made_from, palette)) if Arc::ptr_eq(made_from, theme) => palette.clone(),
            _ => {
                let palette = Arc::new(theme.palette());
                *made = Some((theme.clone(), palette.clone()));
                palette
            }
        }
    }

    fn new(ctx: &egui::Context, options: &mut Options, icon: &egui::IconData) -> Self {
        let (voice_tx, voice_rx) = std::sync::mpsc::channel();
        Self {
            ui: atomics_ui::Client {
                icon: Some(ctx.load_texture("atomisk", egui::ColorImage::from(icon), egui::TextureOptions::LINEAR)),
                ..Default::default()
            },
            ctx: ctx.clone(),
            palette_cache: Default::default(),
            project_picker: None,
            cleanup_open: false,
            cleanup_selected: HashSet::new(),
            cleanup_sandboxes: HashSet::new(),
            cleanup_delete_branches: true,
            plan_cleanup_open: false,
            plan_cleanup_selected: HashSet::new(),
            terminal_views: HashMap::new(),
            new_dialog: None,
            rename: None,
            quick_open: None,
            voice: Voice::Idle,
            github: github::GithubUi::default(),
            #[cfg(test)]
            connection_name: String::new(),
            #[cfg(test)]
            connection_url: String::new(),
            show_settings: None,
            keyboard_idle: false,
            voice_tx,
            voice_rx,
            screenshot: options.screenshot.take().map(|path| Screenshot {
                path,
                at: options.screenshot_delay as f64,
                requested: false,
                done: Arc::new(AtomicBool::new(false)),
            }),
        }
    }
}

/// The desktop's code names the fields that moved to atomics-ui as before.
impl std::ops::Deref for Client {
    type Target = atomics_ui::Client;
    fn deref(&self) -> &atomics_ui::Client {
        &self.ui
    }
}
impl std::ops::DerefMut for Client {
    fn deref_mut(&mut self) -> &mut atomics_ui::Client {
        &mut self.ui
    }
}

/// A short-lived desktop borrow. What the user asks for is a [`Command`](atomics_ui::Command), carried out
/// by [`Desktop::send`]. It is also the shim between the draw code that moved to atomics-ui and the host's
/// model: [`Desktop::draw`] runs that code over the in-process [`Backend`] and then sends the commands it
/// recorded.
struct Desktop<'a> {
    runtime: &'a mut State,
    client: &'a mut Client,
}

impl Desktop<'_> {
    /// The colors and fonts to draw with: the host's theme, as this client's palette.
    fn palette(&self) -> Arc<Palette> {
        self.client.palette(&self.runtime.theme)
    }
}

impl std::ops::Deref for Desktop<'_> {
    type Target = State;
    fn deref(&self) -> &State {
        self.runtime
    }
}
impl std::ops::DerefMut for Desktop<'_> {
    fn deref_mut(&mut self) -> &mut State {
        self.runtime
    }
}

pub struct App {
    client: Client,
    host: crate::host::Host,
    replies: Vec<(bool, Receiver<ControlOutcome>)>,
}

impl App {
    fn ui_with_picker(&mut self, ui: &mut egui::Ui, pick: impl FnOnce(&Path) -> Option<PathBuf>) {
        let mut state = self.host.state();
        Desktop { runtime: &mut state, client: &mut self.client }.paint(ui);
        if state.sessions.iter().any(|s| s.status == AgentStatus::Working)
            || matches!(self.client.voice, Voice::Recording(_))
        {
            ui.ctx().request_repaint_after(PULSE_STEP);
        }
        let commands = std::mem::take(&mut state.client_commands);
        let actions = state.action_tx.clone();
        drop(state);
        for command in commands {
            let start = matches!(command, ControlRequest::Start { .. });
            self.replies.push((start, self.host.command(command)));
        }
        if ui.input(|input| !input.events.is_empty()) {
            self.host.wake();
        }
        // Native dialogs may block for minutes. The runtime must remain free
        // to service hooks, approvals and waits throughout that time.
        if let Some(start) = self.client.project_picker.take()
            && let Some(root) = pick(&start)
        {
            let _ = actions.send(Box::new(move |state| state.open_project(&root)));
        }
    }

    pub fn new(cc: &eframe::CreationContext<'_>, mut options: Options, icon: &egui::IconData) -> Self {
        #[cfg(target_os = "macos")]
        crate::open_link::macos::wake_with(&cc.egui_ctx);
        let client = Client::new(&cc.egui_ctx, &mut options, icon);
        let state = State::new(cc, options, icon);
        Self { host: state.into_host(), client, replies: Vec::new() }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut state = self.host.state();
        self.replies.retain(|(start, reply)| match reply.try_recv() {
            Ok(ControlOutcome::Done(response)) => {
                Desktop { runtime: &mut state, client: &mut self.client }.control_reply(*start, response);
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            _ => false,
        });
        Desktop { runtime: &mut state, client: &mut self.client }.client_logic(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.ui_with_picker(ui, |start| {
            rfd::FileDialog::new().set_title("Open project").set_directory(start).pick_folder()
        });
    }

    fn on_exit(&mut self) {
        self.host.stop();
        self.host.state().shutdown();
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The desktop's window as the runtime sees it. egui stays in the desktop: the runtime only
/// asks for repaints, focus and the window's size through [`crate::host::Surface`].
impl crate::host::Surface for egui::Context {
    fn repaint(&self) {
        self.request_repaint();
    }

    fn repaint_after(&self, delay: Duration) {
        self.request_repaint_after(delay);
    }

    fn raise(&self) {
        self.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        self.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn viewport(&self) -> crate::host::Viewport {
        self.input(|input| {
            let content = input.content_rect().size();
            let viewport = input.viewport();
            crate::host::Viewport {
                content: [content.x, content.y],
                monitor: viewport.monitor_size.map(|size| [size.x, size.y]),
                inner: viewport.inner_rect.map(|rect| [rect.width(), rect.height()]),
                maximized: viewport.maximized.unwrap_or(false),
            }
        })
    }
}

impl State {
    pub(crate) fn into_host(self) -> crate::host::Host {
        let (wake, commands) = (self.wake.clone(), self.control_tx.clone());
        crate::host::Host::start(self, wake, commands)
    }

    /// The desktop's model: its egui window attached and the theme applied to it.
    pub fn new(cc: &eframe::CreationContext<'_>, options: Options, _icon: &egui::IconData) -> State {
        let state = State::build(options, crate::host::Window::attach(cc.egui_ctx.clone()));
        state.theme.palette().apply(&cc.egui_ctx, &SystemFonts);
        state
    }

    /// The runtime model and its services, with `window` attached or (headless) none. Nothing here
    /// needs egui, a display or a GPU.
    pub(crate) fn build(options: Options, window: crate::host::Window) -> State {
        std::thread::spawn(|| LazyLock::force(&SHELL_PATH));
        // Before any git runs in a folder a sandbox has worked in. Records of sandboxes that are gone go after.
        crate::sandbox::load_pins(&paths::git_pins_dir());
        let sbx = if cfg!(test) { "/nonexistent/atomics-test-sbx" } else { "sbx" };
        let config_path = paths::config_file();
        // No config yet is a first run, and it stays one until the setup screen is finished or skipped. A config someone
        // named belongs to a test, a QA instance or a managed host, whose runs nobody sets up by hand.
        let first_run = !config_path.exists() && !paths::config_named();
        let (mut config, config_error) = match Config::load_or_create(&config_path) {
            Ok(config) => (config, None),
            Err(err) => (Config::default(), Some(format!("{err:#}"))),
        };
        with_remote_port(&mut config, options.remote_port);
        let config_problems = github::config_problems(&config);
        let (theme, theme_errors) = Theme::load(&config.theme, &theme::themes_dir(&config_path));
        let theme_error = theme_error(&theme_errors);

        let hub = Arc::new(Mutex::new(Hub::default()));
        let wake = crate::host::Wake::default();
        let (hook_tx, hook_rx) = wake.channel();
        let (control_tx, control_rx) = wake.channel();
        let (op_tx, op_rx) = wake.channel();
        let (action_tx, action_rx) = wake.channel();
        let (usage, usage_tx) = crate::usage::UsagePoller::new(window.clone());
        let sandbox_pull = crate::usage::SandboxPull::new(usage_tx.clone());
        let vm = crate::vm::Monitor::start(window.clone());
        let governor = Arc::new(Governor::new(config.resources.clone(), window.clone()));
        let channels =
            Channels { hooks: hook_tx, control: control_tx.clone(), usage: usage_tx, governor: governor.clone() };
        let hook_error =
            ipc::serve(&paths::socket_path(), hub.clone(), channels, window.clone()).err().map(|e| format!("{e:#}"));
        // Only the instance that owns the socket owns the pool, so a second ATOMICS never replaces it.
        let mut notices = Vec::new();
        let jobserver = match hook_error {
            Some(_) => None,
            None => {
                // Only the instance that owns the socket records its runs, so second instances stay out of the log.
                notices.extend(crate::crash::started(&paths::atomics_home()));
                // The hooks bare Codex agents get run this executable: the doctor checks Codex's trust for it.
                if let Err(error) = crate::setup::record_executable(&paths::atomics_home()) {
                    eprintln!("atomics: recording this executable for the doctor failed: {error}");
                }
                let tokens = config.resources.jobs(governor.cores);
                match (tokens > 0).then(|| Jobserver::create(&Jobserver::path_for(&paths::atomics_home()), tokens)) {
                    None => None,
                    Some(Ok(jobserver)) => {
                        let jobserver = Arc::new(jobserver);
                        jobserver.refill_when_idle();
                        Some(jobserver)
                    }
                    Some(Err(err)) => {
                        notices.push(format!("Compiler job pool unavailable: {err:#}"));
                        None
                    }
                }
            }
        };
        let (watch_tx, watch_rx) = wake.channel();
        let (worktree_tx, worktree_rx) = wake.channel();
        let (integrate_tx, integrate_rx) = wake.channel();
        let (integration_check_tx, integration_check_rx) = wake.channel();
        let launch_dir = std::env::current_dir().unwrap_or_else(|_| paths::home());
        let tickets = tickets::Tickets::new(&wake);

        let (mut plans, plan_error) = match plan::load_plans(&paths::atomics_home()) {
            Ok((plans, errors)) => {
                (plans, (!errors.is_empty()).then(|| format!("Unreadable plan files:\n{}", errors.join("\n"))))
            }
            Err(err) => (Vec::new(), Some(format!("Loading plans failed: {err:#}"))),
        };
        let selected_plan = plans.iter().position(Plan::is_open).unwrap_or_else(|| {
            // The empty board before the first plan is created is never saved.
            plans.push(Plan::default());
            plans.len() - 1
        });
        let update = update::Updates::new(&config);
        let mut app = State {
            window,
            remote: crate::host::RemoteView::default(),
            folder_cache: Vec::new(),
            management_config: std::sync::OnceLock::new(),
            credential_status: serde_json::Value::Null,
            credential_busy: false,
            remote_port: options.remote_port,
            wake: wake.clone(),
            control_tx,
            client_commands: Vec::new(),
            client_events: Vec::new(),
            action_tx,
            action_rx,
            highlighter: Arc::new(Highlighter::new(&theme.syntax)),
            theme: Arc::new(theme),
            config_stamp: config_fingerprints(&config_path),
            repo_workflows: HashMap::new(),
            project_roots: HashMap::new(),
            config,
            config_path,
            config_checked: Instant::now(),
            config_error,
            config_problems,
            theme_error,
            host_theme: false,
            themes: Default::default(),
            sessions: Vec::new(),
            previews: Default::default(),
            machine: Default::default(),
            codex_versions: HashMap::new(),
            pending_codex: Vec::new(),
            // Tests never reach the sbx installed on the machine running them.
            sbx: sbx.into(),
            sandbox_check: None,
            sandbox_lines: Vec::new(),
            sandbox_steps: Vec::new(),
            programs_check: None,
            setup_programs: Vec::new(),
            setup_azure: Default::default(),
            setup_azure_install: None,
            setup_job: None,
            setup_failure: None,
            setup_look_again: false,
            install_offer: crate::install::offer(),
            launch_project: workspace::project_root(&launch_dir).is_ok(),
            start_choice: None,
            interactive_start: false,
            pending_sandbox: Vec::new(),
            lifecycle_jobs: Vec::new(),
            deferred_cleanup: crate::lifecycle::deferred(&paths::atomics_home()),
            deferred_checked: None,
            deferred_checking: Arc::default(),
            cleaning_plan: None,
            startup_cleanup: None,
            shutting_down: false,
            ref_checks: crate::ref_guard::Checks::new(),
            identity_updates: crate::ref_guard::Checks::new(),
            sandbox_usage: Default::default(),
            relay: Arc::new(crate::relay::Service::new(paths::atomics_home(), crate::relay::Keychain::current())),
            connections: Default::default(),
            github: Default::default(),
            active: None,
            diff_open: true,
            diff_root: None,
            diffs: HashMap::new(),
            traces: HashMap::new(),
            worker: DiffWorker::new(wake),
            hub,
            hook_rx,
            usage,
            vm,
            sandbox_pull,
            control_rx,
            op_tx,
            op_rx,
            ops: crate::protocol::journal::Journal::memory(),
            in_flight: Vec::new(),
            op_replies: Vec::new(),
            resolved: Default::default(),
            hook_error,
            governor,
            jobserver,
            resources: Default::default(),
            resources_checked: Instant::now() - Duration::from_secs(10),
            update,
            busy_checked: Instant::now(),
            output_delay: None,
            approvals_checked: Instant::now() - APPROVAL_CHECK_INTERVAL,
            needs_input_checked: Instant::now() - NEEDS_INPUT_CHECK_INTERVAL,
            store: {
                let mut store = Store::load(&paths::store_file());
                if first_run {
                    store.setup_pending = true;
                    if let Err(error) = store.save(&paths::store_file()) {
                        eprintln!("ATOMICS: could not save the first run: {error:#}");
                    }
                }
                store
            },
            dismissed: HashSet::new(),
            repos: Vec::new(),
            open_projects: Vec::new(),
            workspace: options.workspace.clone(),
            workspace_names: Vec::new(),
            worktree_project: HashMap::new(),
            git_identities: HashMap::new(),
            title: String::new(),
            cleanup: None,
            plan_cleanup: None,
            panel: PanelMode::Diff,
            editors: HashMap::new(),
            editor_files: HashMap::new(),
            events: Vec::new(),
            event_seq: 0,
            last_status: HashMap::new(),
            waiters: Vec::new(),
            heartbeat: heartbeat::Heartbeat::default(),
            chief_wake_cursor: 0,
            agent_cleanup: agent_cleanup::Checks::default(),
            tickets,
            plans,
            selected_plan,
            changed_plans: HashSet::new(),
            view: None,
            plan_view: false,
            ungrouped: false,
            chief_open: false,
            chief_autostart: false,
            worktree_tx,
            worktree_rx,
            creating: Vec::new(),
            integrate_tx,
            integrate_rx,
            integrating: HashSet::new(),
            integration_check_tx,
            integration_check_rx,
            integration_checks: HashMap::new(),
            integration_check_id: 0,
            integration_checked: HashMap::new(),
            integration_retries: HashMap::new(),
            integration_problems: HashMap::new(),
            identity_context: None,
            dispatch_waiters: Vec::new(),
            integration_dirty: Arc::new(AtomicBool::new(false)),
            integration_revision: 0,
            worktrees: Vec::new(),
            worktrees_at: None,
            worktree_listing: None,
            focus_terminal: None,
            last_layout: None,
            watcher: None,
            watch_tx,
            watch_rx,
            toast: None,
            pending_link: options.link,
            pending_link_since: None,
            chief_draft: None,
            pending_open: options.open,
            resume: Vec::new(),
            restarting: false,
            restart_event: None,
            restart_layout: None,
            plan_edit_open: false,
            pending_questions: HashSet::new(),
            proposals: Vec::new(),
            proposal_seq: 0,
            desktop_split: None,
            desktop_diff_split: None,
            launch_dir,
        };
        // The view the window's switcher showed last time; a restart's own state wins below.
        app.panel = match app.store.panel_view.as_deref() {
            Some("board") => PanelMode::Plan,
            Some("tickets") => PanelMode::Tickets,
            _ => PanelMode::Diff,
        };
        app.refresh_themes();
        app.open_initial_projects(options.projects, options.workspace);
        app.refresh_workspace_names();
        app.apply_config();
        if app.hook_error.is_none() {
            app.begin_startup_cleanup();
            app.restore_restart();
            app.chief_autostart = app.chief_autostarts();
        }
        // One toast for everything startup has to say, so no notice hides another.
        notices.splice(0..0, options.migration_errors.iter().cloned());
        notices.extend(plan_error);
        if !notices.is_empty() {
            app.toast(notices.join("\n"));
        }
        app
    }

    fn apply_config(&mut self) {
        self.management_config.take();
        for index in 0..self.plans.len() {
            if self.plans[index].is_open() && self.plans[index].select_review_agent(&self.config) {
                self.save_plan(index);
            }
        }
        if !self.effective_isolation().sandbox.azure.enabled {
            self.relay.revoke_azure();
        }
        let mut hub = self.hub.lock().unwrap();
        hub.session_context = self.config.fill(&self.config.steer.session_context);
        hub.external_prefix = self.config.steer.external_prefix.clone();
        drop(hub);
        self.governor.set_config(self.config.resources.clone());
        let mut repos = self.open_projects.clone();
        for repo in &self.config.repos {
            let root = workspace::project_root(&paths::expand(repo)).unwrap_or_else(|_| paths::expand(repo));
            if !repos.contains(&root) {
                repos.push(root);
            }
        }
        self.repos = repos;
        self.reload_repo_workflows();
        self.worktrees_at = None;
        self.apply_ticket_config();
        self.refresh_worktree_identities();
    }

    fn resources_json(&self) -> serde_json::Value {
        let (running, waiting) = self.governor.snapshot();
        json!({
            "cores": self.governor.cores,
            "load": crate::governor::load_average(),
            "jobs": self.jobserver.as_ref().map(|j| j.tokens()),
            "slots": { "test": self.governor.slots(SlotClass::Test), "qa": self.governor.slots(SlotClass::Qa) },
            "running": running,
            "waiting": waiting,
        })
    }

    fn toast(&mut self, text: impl Into<String>) {
        let text = text.into();
        // The headless host has nowhere to show a toast; its log is stderr.
        if !self.window.is_attached() {
            eprintln!("atomics host: {text}");
        }
        self.toast = Some((text, Instant::now()));
    }

    pub(crate) fn window(&self) -> &crate::host::Window {
        &self.window
    }

    /// Why the local socket could not be served, when it could not.
    pub(crate) fn hook_error(&self) -> Option<&str> {
        self.hook_error.as_deref()
    }

    /// With no window attached nothing consumes client events. Answer the ones a caller waits on
    /// and drop the rest, so they never pile up in the headless host.
    pub(crate) fn drop_client_events(&mut self) {
        for event in self.client_events.drain(..) {
            if let ClientEvent::Screenshot { reply, .. } = event {
                let _ = reply.send(ControlOutcome::Done(ControlResponse::error(NO_WINDOW)));
            }
        }
        self.client_commands.clear();
    }

    fn active_index(&self) -> Option<usize> {
        let id = self.active.as_ref()?;
        self.sessions.iter().position(|s| &s.id == id)
    }

    fn diff_target(&self) -> Option<PathBuf> {
        match self.active_index() {
            Some(i) => Some(self.sessions[i].root()),
            None => self.diff_root.clone(),
        }
    }

    /// Makes a session the active one. The diff panel follows it.
    fn activate(&mut self, id: &str, open_diff: bool) {
        self.active = Some(id.to_string());
        self.diff_root = None;
        if open_diff {
            self.diff_open = true;
        }
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == id) {
            if s.status == AgentStatus::Done {
                s.status = AgentStatus::Idle;
            }
            if s.terminal.is_some() {
                self.focus_terminal = Some(id.to_string());
                self.window.request_repaint();
            }
        }
    }

    // ── drawing ───────────────────────────────────────────────────────────
}

impl State {
    fn begin_startup_cleanup(&mut self) {
        let (home, config, sbx, actions) =
            (paths::atomics_home(), self.config.clone(), self.sbx.clone(), self.action_tx.clone());
        // Before the sweep thread and restored agents start, so nothing they create is a candidate.
        let cutoff = crate::lifecycle::Cutoff::mark(&home);
        // Read before the handoff is consumed: the scratch sessions a restart resumes keep their folders.
        let resumed: HashSet<PathBuf> =
            self.store.restart.iter().flat_map(|r| &r.sessions).filter_map(|r| paths::scratch_of(&r.cwd)).collect();
        self.startup_cleanup = Some(std::thread::spawn(move || {
            let sbx = resolve_program(&sbx);
            let kept = crate::lifecycle::kept_leftovers(&home);
            let scratch_before = cutoff.as_ref().ok().map(crate::lifecycle::Cutoff::files);
            let swept = cutoff.and_then(|cutoff| crate::lifecycle::sweep_before(&home, &config, &sbx, cutoff));
            let pins_loaded = swept.is_ok();
            let mut notes = swept.unwrap_or_else(|err| vec![format!("Startup cleanup skipped: {err:#}")]);
            if let Some(before) = scratch_before {
                notes.extend(crate::lifecycle::sweep_scratch(&home, &resumed, before));
            }
            crate::sandbox::prune_pins(&home, &home.join("git-pins"), &sbx);
            // Replaces the previous start's notes: what this sweep no longer finds is fixed.
            crate::lifecycle::record_sweep(&home, &notes, pins_loaded, &kept);
            if !notes.is_empty() {
                let _ = actions.send(Box::new(move |state| state.toast(notes.join("\n"))));
            }
        }));
    }

    /// Starts lifecycle work in the background; see [`crate::lifecycle::Job`].
    pub(super) fn lifecycle(&mut self, scope: Scope, work: impl FnOnce() + Send + 'static) {
        self.lifecycle_jobs.retain(|job| !job.is_finished());
        self.lifecycle_jobs.push(crate::lifecycle::Job::spawn(scope, work));
    }

    /// The lifecycle jobs still running, for a background thread to wait for what it must.
    pub(super) fn running_lifecycle(&mut self) -> Vec<crate::lifecycle::Job> {
        self.lifecycle_jobs.retain(|job| !job.is_finished());
        self.lifecycle_jobs.clone()
    }

    /// Waits for every lifecycle job. Tests only: the UI thread never waits for them, and a quit
    /// waits a bounded time.
    #[cfg(test)]
    fn finish_lifecycle_jobs(&mut self) {
        for job in self.lifecycle_jobs.drain(..) {
            job.wait();
        }
    }

    pub(crate) fn shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        // macOS never returns from the event loop after Cmd+Q, so the clean exit is recorded here.
        crate::crash::exited(&paths::atomics_home());
        self.stop_tickets();
        for editor in self.editors.values() {
            editor.shutdown();
        }
        for session in &self.sessions {
            if let Some(terminal) = &session.terminal {
                terminal.shutdown();
            }
        }
        // Preparation can finish after the window closes. Wait before stopping those sandboxes, but not
        // for sbx to make one, which can take 15 minutes: that stops, and its preparation removes what
        // sbx made of it.
        let mut names: HashSet<String> = self.sessions.iter().filter_map(|s| s.sandbox.clone()).collect();
        let mut scratch: Vec<(PathBuf, Option<String>)> =
            self.sessions.iter().filter_map(|s| Some((s.scratch.clone()?, s.sandbox.clone()))).collect();
        let mut pending = std::mem::take(&mut self.pending_sandbox);
        for round in 0..2 {
            let deadline = Instant::now() + QUIT_WAIT;
            pending.retain(|(launch, ready)| {
                let left = deadline.saturating_duration_since(Instant::now());
                if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = ready.recv_timeout(left) {
                    return true;
                }
                if let AgentRuntime::Sandbox(sandbox) = &launch.runtime {
                    if let Some(dir) = paths::scratch_of(&launch.cwd) {
                        scratch.push((dir, Some(sandbox.name.clone())));
                    }
                    names.insert(sandbox.name.clone());
                }
                false
            });
            // Also one a closed plan's cleanup waits for, which is not pending here.
            if round == 0 {
                crate::sandbox::stop_creating();
            }
        }
        // A scratch folder whose sandbox is still being made goes too; its preparation removes what
        // sbx made of the sandbox.
        scratch.extend(pending.iter().filter_map(|(launch, _)| Some((paths::scratch_of(&launch.cwd)?, None))));
        let mut notes: Vec<String> = pending
            .iter()
            .filter_map(|(launch, _)| match &launch.runtime {
                AgentRuntime::Sandbox(sandbox) => {
                    Some(format!("Sandbox {} was still being made at quit", sandbox.name))
                }
                AgentRuntime::Bare => None,
            })
            .collect();
        for name in names {
            self.stop_sandbox(name);
        }
        // Work still running after that ends with ATOMICS, as in a crash; the next start's sweep finds
        // what it left.
        let deadline = Instant::now() + QUIT_JOBS_WAIT;
        let unfinished = self.lifecycle_jobs.drain(..).filter(|job| !job.wait_until(deadline)).count();
        if unfinished > 0 {
            notes.push(format!("{unfinished}{}", crate::lifecycle::UNFINISHED_AT_QUIT));
        }
        crate::lifecycle::record_leftovers(&paths::atomics_home(), &notes);
        // Quitting ends scratch sessions for good; a restart resumes them in their folders.
        if !self.restarting {
            self.remove_scratch_sessions(scratch);
        }
        self.check_all_refs(&self.exempt_branches());
        self.ref_checks.finish();
        self.identity_updates.finish();
        if let Some(job) = self.startup_cleanup.take() {
            let _ = job.join();
        }
        // A second instance never owned the socket; removing it would cut the first off.
        if self.hook_error.is_none() {
            let _ = std::fs::remove_file(paths::socket_path());
        }
    }

    /// Removes scratch folders and the sandboxes that worked in them, once their agents are stopped.
    fn remove_scratch_sessions(&mut self, scratch: Vec<(PathBuf, Option<String>)>) {
        let (home, sbx) = (paths::atomics_home(), resolve_program(&self.sbx));
        let mut notes = Vec::new();
        for (dir, sandbox) in scratch {
            let removed = sandbox
                .map_or(Ok(()), |name| crate::sandbox::remove(&home, &sbx, &name))
                .and_then(|()| crate::lifecycle::remove_scratch(&home, &dir));
            notes.extend(removed.err().map(|err| format!("Scratch session {}: {err:#}", dir.display())));
        }
        crate::lifecycle::record_leftovers(&home, &notes);
    }
}

/// `atomics host --remote-port`: the protocol server on that port, on loopback as always, whatever the
/// config file says, so a service can rely on it (the NixOS VM's host, docs/nixos-vm.md).
pub(crate) fn with_remote_port(config: &mut Config, port: Option<u16>) {
    if let Some(port) = port {
        config.remote.enabled = true;
        config.remote.port = port;
    }
}

fn state_refresh(state: &mut DiffData) {
    state.dirty_since = Some(Instant::now() - Duration::from_secs(1));
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// The config files and, last, every theme file, so editing a theme reloads it too.
fn config_fingerprints(path: &Path) -> [Option<u64>; 3] {
    let [config, host] = Config::source_paths(path).map(|file| file_fingerprint(&file));
    [config, host, themes_fingerprint(&theme::themes_dir(path))]
}

fn themes_fingerprint(dir: &Path) -> Option<u64> {
    use std::hash::{DefaultHasher, Hash, Hasher};

    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|extension| extension == "toml"))
        .collect();
    files.sort();
    let mut hasher = DefaultHasher::new();
    for file in files {
        file.hash(&mut hasher);
        file_fingerprint(&file).hash(&mut hasher);
    }
    Some(hasher.finish())
}

/// Theme problems, shown in the top bar beside config errors.
fn theme_error(errors: &[String]) -> Option<String> {
    (!errors.is_empty()).then(|| errors.join("; "))
}

/// A small config file's content as a hash, `None` when it cannot be read. Rapid edits
/// can share a modification timestamp on Linux filesystems, so polling compares content.
/// These files are polled once per second off the UI thread.
fn file_fingerprint(file: &Path) -> Option<u64> {
    use std::hash::{DefaultHasher, Hash, Hasher};

    let bytes = std::fs::read(file).ok()?;
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some(hasher.finish())
}

impl Desktop<'_> {
    fn apply_client_events(&mut self, ctx: &egui::Context) {
        if let Some(layout) = self.runtime.restart_layout.take() {
            self.client.split = layout.split;
            self.client.diff_split = layout.diff_split;
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(layout.maximized));
            if !layout.maximized
                && let Some([width, height]) = layout.window_size
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(vec2(width, height)));
            }
        }
        if self.runtime.restarting {
            self.client.confirm_close = None;
            self.client.confirm_plan_close = None;
        }
        if let Some(id) = self.runtime.focus_terminal.take() {
            self.client.focus_terminal = Some(id);
        }
        for event in std::mem::take(&mut self.runtime.client_events) {
            match event {
                ClientEvent::ApprovalDecided { session, approval } => {
                    self.client.approval_decisions.remove(&(session, approval));
                }
                ClientEvent::Cleanup { preselect } => {
                    self.client.cleanup_open = true;
                    self.client.cleanup_selected = preselect.into_iter().collect();
                    self.client.cleanup_sandboxes.clear();
                    self.client.cleanup_delete_branches = true;
                }
                ClientEvent::PlanCleanup => {
                    self.client.plan_cleanup_open = true;
                    self.client.plan_cleanup_selected.clear();
                }
                ClientEvent::PlanCleanupPreview(selected) => {
                    self.client.plan_cleanup_selected = selected.into_iter().collect()
                }
                ClientEvent::OpenFile { root, path, whole_file } => {
                    self.client.diff_views.entry(root).or_default().open_resolved(&path, whole_file);
                }
                ClientEvent::Screenshot { path, reply } => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaintWhileHidden);
                    let path = paths::expand(&path);
                    let reply = reply.clone();
                    ctx.request_screenshot(move |image| {
                        let response = match save_png(&path, &image) {
                            Ok(()) => ControlResponse::ok(json!({ "path": path, "size": image.size })),
                            Err(err) => ControlResponse::error(format!("{err:#}")),
                        };
                        let _ = reply.send(ControlOutcome::Done(response));
                    });
                    ctx.request_repaint();
                }
                ClientEvent::ConfigChanged => self.palette().apply(ctx, &SystemFonts),
                ClientEvent::Viewport(command) => ctx.send_viewport_cmd(command),
                ClientEvent::ShowSettings(open) => self.client.show_settings = Some(open),
                ClientEvent::VoiceFailed { text, error } => self.voice_failed(&text, &error),
                ClientEvent::PullRequest(key) => {
                    self.client.ui.pr_view = Some((key, DiffState::default()));
                    self.client.ui.pr_target = None;
                }
                ClientEvent::Edit { root, path } => {
                    self.client.ui.focus_editor = true;
                    if let Some(path) = path {
                        self.client.editor_file.insert(root, path);
                    }
                }
            }
        }
    }

    fn sync_diffs(&mut self) {
        self.draw(atomics_ui::diff_panel::sync);
    }

    fn client_logic(&mut self, ctx: &egui::Context) {
        self.sync_diffs();
        self.apply_client_events(ctx);
        while let Ok(event) = self.client.voice_rx.try_recv() {
            match event {
                VoiceEvent::Opened(cancelled, result) => {
                    if matches!(&self.client.voice, Voice::Opening(current) if Arc::ptr_eq(current, &cancelled)) {
                        match result {
                            Ok(recorder) => self.client.voice = Voice::Recording(recorder),
                            Err(err) => {
                                self.client.voice = Voice::Idle;
                                self.runtime.toast(format!("Voice: {err}"));
                            }
                        }
                    }
                }
                VoiceEvent::Transcript(result) => {
                    self.client.voice = Voice::Idle;
                    match result {
                        Ok(text) => self.send_voice(ctx, &text),
                        Err(err) => self.runtime.toast(format!("Voice: {err}")),
                    }
                }
            }
        }
        if let Voice::Recording(recorder) = &self.client.voice
            && recorder.elapsed() >= MAX_RECORDING
        {
            self.stop_voice();
            self.runtime.toast("Voice: stopped after 5 minutes, transcribing");
        }

        self.screenshot(ctx);
        for terminal in
            self.runtime.sessions.iter().filter_map(|s| s.terminal.as_ref()).chain(self.runtime.editors.values())
        {
            if let Some(text) = terminal.shared.clipboard.lock().unwrap().take() {
                ctx.copy_text(text);
            }
        }
    }

    fn paint(&mut self, ui: &mut egui::Ui) {
        self.runtime.plan_edit_open = false;
        self.sync_diffs();
        self.apply_client_events(ui.ctx());
        if self.runtime.restarting {
            return;
        }
        let ctx = ui.ctx().clone();
        self.shortcuts(&ctx);
        atomics_ui::shell::frame(ui, self);
        self.runtime.desktop_split = self.client.split;
        self.runtime.desktop_diff_split = self.client.diff_split;
        self.dialogs(&ctx);
        self.github_modal(&ctx);
        // egui lets go of a text field on Escape before the next frame's shortcuts see the key.
        self.client.keyboard_idle = ctx.memory(|m| m.focused().is_none());
        atomics_ui::panel_nav::settle_focus(&ctx);
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod board_tests;

#[cfg(test)]
mod key_tests;

#[cfg(test)]
mod host_tests;

#[cfg(test)]
mod client_tests;

#[cfg(test)]
mod sandbox_tests;

#[cfg(test)]
mod setup_tests;

#[cfg(test)]
mod ungrouped_tests;
