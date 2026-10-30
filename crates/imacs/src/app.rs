use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::sync::mpsc::channel;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use egui::{Color32, FontId, Key, Pos2, Rect, RichText, Sense, Stroke, Vec2, pos2, vec2};
use imacs_core::config::{AgentKind, ChiefDock, Config, VoiceTarget};
use imacs_core::git::{self, Integration, Merge, Worktree, WorktreeEntry};
use imacs_core::layout::{self, DiffPlacement, DiffSplit, Layout};
use imacs_core::session::{AgentStatus, pick_tiled, title_for};
use imacs_core::store::{SessionRecord, Store};
use imacs_core::{paths, steer, theme};
use notify_debouncer_mini::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer};

use crate::diff_view::{self, DiffAction, DiffState, ExplorerState, Header};
use crate::diff_worker::{DiffData, DiffWorker, WorkerResult};
use crate::governor::{Governor, Jobserver};
use crate::highlight::Highlighter;
use crate::ipc::{self, Channels, ControlMessage, ControlOutcome, HookMessage, Hub};
use crate::lifecycle::Scope;
use crate::sandbox::AgentRuntime;
use crate::setup::{SHELL_PATH, env_var, global_hooks_installed, install_global_hooks, resolve_program};
use crate::terminal::{Metrics, Terminal, TerminalView};
use crate::theme::{Palette, lerp, shortcut, shortcut_label};
use crate::voice::{MAX_RECORDING, Recorder};
use imacs_core::approvals::{ApprovalRequest, Decision};
use imacs_core::control::{ControlRequest, ControlResponse};
use imacs_core::governor::{SlotClass, Ticket};
use imacs_core::hook::HookReply;
use imacs_core::plan::{self, Plan, PlanStatus, Report, Task, TaskStatus, short_slug};
use imacs_core::workspace::{self, Workspace};
use serde_json::json;

mod approvals;
mod background;
mod board;
mod chief;
mod commands;
mod control;
mod editor;
mod fleet;
mod heartbeat;
mod input;
mod integrations;
mod plan_cleanup;
mod plan_view;
mod plans;
mod projects;
mod ref_guard;
mod repo_workflow;
mod restart;
mod sandbox_settings;
mod selfconfig;
mod sessions;
mod sidebar;
mod themes;
mod tickets;
mod topbar;
mod voice;
mod workflow;
mod worktrees;

use chief::{CHIEF_ALLOWED_TOOLS, chief_divider};
use editor::save_png;
use fleet::queue_note;
use plan_view::PlanState;

const TAB_HEIGHT: f32 = 30.0;
const TILE_HEADER: f32 = 26.0;
const WORKTREE_REFRESH: Duration = Duration::from_secs(30);
const BACKGROUND_REFRESH: Duration = Duration::from_secs(5);
const FALLBACK_REFRESH: Duration = Duration::from_secs(30);
const PULSE_STEP: Duration = Duration::from_millis(250);
/// Untracked content kept for a diff off screen; the rest is counted, not loaded.
const OFF_SCREEN_UNTRACKED: u64 = 1024 * 1024;

/// Folders whose churn (builds, dependencies, git internals) never changes the diff.
const IGNORED_DIRS: &[&str] = &[".git", "node_modules", "target", "bin", "obj", ".idea", ".vs", "dist"];

#[derive(Default)]
pub struct Options {
    /// Projects to open, from `imacs <path>`; empty means the launch folder or the last session.
    pub projects: Vec<PathBuf>,
    pub workspace: Option<String>,
    pub open: Vec<(String, PathBuf)>,
    pub screenshot: Option<PathBuf>,
    pub screenshot_delay: f32,
    pub migration_errors: Vec<String>,
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
    /// The chief agent stays outside the agent fleet.
    pub chief: bool,
    /// A message typed into the terminal that the agent has not visibly picked up yet.
    pending: Option<Pending>,
    /// The sandbox a sandboxed agent runs in.
    sandbox: Option<String>,
    screen_busy: bool,
    busy_absent_since: Option<Instant>,
    ref_guard: Option<crate::ref_guard::Shared>,
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
}

/// Who a session is started as: the chief, or an agent with its plan task's own isolation, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Chief,
    Agent(Option<imacs_core::isolation::IsolationMode>),
}

struct AgentLaunch {
    id: String,
    agent: imacs_core::config::AgentConfig,
    cwd: PathBuf,
    resume: Option<String>,
    extra_args: Vec<String>,
    prompt: Option<String>,
    chief: bool,
    runtime: AgentRuntime,
    /// A sandboxed session's ref guard, opened and checked before its agent starts.
    ref_guard: Option<crate::ref_guard::Shared>,
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

const APPROVAL_CHECK_INTERVAL: Duration = Duration::from_millis(250);
const APPROVAL_MENU_GRACE: Duration = Duration::from_secs(1);
const NEEDS_INPUT_CHECK_INTERVAL: Duration = Duration::from_secs(30);

const DELIVERY_TIMEOUT: Duration = Duration::from_secs(4);
const DELIVERY_ATTEMPTS: u8 = 3;
const CODEX_VERSION_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a quit waits for sandboxes being prepared, and then again for those whose creation it
/// stopped to clean up.
const QUIT_WAIT: Duration = Duration::from_secs(10);
/// How long a quit waits for sandbox and worktree cleanup still running.
const QUIT_JOBS_WAIT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, PartialEq, Eq)]
enum SteerTarget {
    Active,
    Agents,
    Selected,
}

struct NewDialog {
    agent: String,
    new_worktree: bool,
    folder: String,
    repo: String,
    branch: String,
    error: Option<String>,
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
    isolation: Option<imacs_core::isolation::IsolationMode>,
}

/// A background worktree job completed all preparation needed before launch.
struct PreparedWorktree {
    path: PathBuf,
    warmed: bool,
}

/// A finished plan integration, to note on its task and answer.
struct Integrated {
    plan: String,
    task: String,
    branch: String,
    into: String,
    result: Result<Integration, String>,
    reply: Option<Sender<ControlOutcome>>,
}

struct CheckedIntegration {
    revision: u64,
    plan: String,
    task: Task,
    into: String,
    /// `None` means git could not verify ancestry; keep the integration marker.
    valid: Option<bool>,
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum PanelMode {
    Diff,
    Editor,
    /// The orchestrator's plan board.
    Plan,
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
    /// Set for a `imacs_report` from the agent: (status, summary, task).
    report: Option<(String, String, Option<String>)>,
    approval: Option<ApprovalRequest>,
    findings: Option<Vec<imacs_core::heartbeat::Finding>>,
    /// Set when someone changed a linked ticket in its system.
    ticket: Option<serde_json::Value>,
    /// Set when the user applied or rejected a proposed config change.
    config: Option<serde_json::Value>,
}

impl StatusEvent {
    /// What `wait` matches on: the agent's status, "reported", "heartbeat", "ticket" or "config".
    fn kind(&self) -> &str {
        if self.findings.is_some() {
            "heartbeat"
        } else if self.ticket.is_some() {
            "ticket"
        } else if self.config.is_some() {
            "config"
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
        if let Some((status, summary, task)) = &self.report {
            value["report"] = json!({ "status": status, "summary": summary, "task": task });
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
        if let Some(request) = &self.approval {
            value["approval"] = json!(request);
        }
        value
    }
}

/// A `wait` request held until a matching event or its deadline.
struct Waiter {
    sessions: Vec<String>,
    statuses: Vec<String>,
    after: u64,
    deadline: Instant,
    reply: Sender<ControlOutcome>,
}

const MAX_WAIT: Duration = Duration::from_secs(280);

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
    ctx: egui::Context,
    wake: crate::host::Wake,
    control_tx: crate::host::Sender<ControlMessage>,
    client_commands: Vec<ControlRequest>,
    client_events: Vec<ClientEvent>,
    action_tx: crate::host::Sender<commands::Action>,
    action_rx: Receiver<commands::Action>,
    config: Config,
    config_path: PathBuf,
    config_stamp: [Option<u64>; 3],
    repo_workflows: HashMap<PathBuf, imacs_core::repo_workflow::RepoWorkflow>,
    /// The project each folder belongs to, asked of git once per folder.
    project_roots: HashMap<PathBuf, PathBuf>,
    config_checked: Instant,
    config_error: Option<String>,
    /// What the theme files or `[theme]` could not apply; the rest of the theme still does.
    theme_error: Option<String>,
    /// The themes the Settings menu offers, read when the config or a theme file changes.
    theme_choices: Arc<Vec<themes::ThemeChoice>>,
    /// This machine's config.d file picks a theme, which wins over config.toml's.
    host_theme: bool,
    palette: Arc<Palette>,
    highlighter: Arc<Highlighter>,
    sessions: Vec<Session>,
    codex_versions: HashMap<String, CodexVersion>,
    pending_codex: Vec<(AgentLaunch, CodexVersion)>,
    /// The `sbx` program sandboxed agents run with.
    sbx: String,
    sandbox_usage: crate::sandbox_usage::Sampler,
    lifecycle_jobs: Vec<crate::lifecycle::Job>,
    startup_cleanup: Option<std::thread::JoinHandle<()>>,
    shutting_down: bool,
    sandbox_check: Option<Receiver<Vec<String>>>,
    sandbox_lines: Vec<String>,
    /// Sandboxed launches waiting for their sandbox, and their ref guard with what its first check found.
    pending_sandbox: Vec<(AgentLaunch, Receiver<Result<crate::ref_guard::Opened, String>>)>,
    ref_checks: crate::ref_guard::Checks,
    active: Option<String>,
    diff_open: bool,
    /// Worktree picked in the sidebar without an agent; the diff otherwise follows the active session.
    diff_root: Option<PathBuf>,
    diffs: HashMap<PathBuf, DiffData>,
    worker: DiffWorker,
    hub: Arc<Mutex<Hub>>,
    hook_rx: Receiver<HookMessage>,
    usage: crate::usage::UsagePanel,
    /// Account usage that sandboxed agents saw, read back from their VMs.
    sandbox_pull: crate::usage::SandboxPull,
    control_rx: Receiver<ControlMessage>,
    hook_error: Option<String>,
    /// Machine-wide limits for the agents' heavy work.
    governor: Arc<Governor>,
    /// Compiler job pool every agent inherits; None when this instance does not own the socket.
    jobserver: Option<Arc<Jobserver>>,
    /// One-minute load average, read at most every 2 seconds.
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
    /// Worktree folder → its project's main checkout.
    worktree_project: HashMap<PathBuf, PathBuf>,
    title: String,
    /// Diff/editor width set by dragging the divider.
    /// Side by side or unified, picked in the diff header; wins over the config until IMACS quits.
    cleanup: Option<Cleanup>,
    plan_cleanup: Option<plan_cleanup::PlanCleanup>,
    panel: PanelMode,
    editors: HashMap<PathBuf, Terminal>,
    events: Vec<StatusEvent>,
    event_seq: u64,
    last_status: HashMap<String, AgentStatus>,
    waiters: Vec<Waiter>,
    heartbeat: heartbeat::Heartbeat,
    tickets: tickets::Tickets,
    plans: Vec<Plan>,
    selected_plan: usize,
    changed_plans: HashSet<String>,
    /// The project tab on screen; `None` shows every project.
    view: Option<PathBuf>,
    plan_view: bool,
    /// The chief is shown, either floating or docked (otherwise only the pill).
    chief_open: bool,
    worktree_tx: crate::host::Sender<(String, Result<PreparedWorktree, String>, Launch, PathBuf)>,
    worktree_rx: Receiver<(String, Result<PreparedWorktree, String>, Launch, PathBuf)>,
    creating: Vec<(PathBuf, String, PathBuf)>,
    integrate_tx: crate::host::Sender<Integrated>,
    integrate_rx: Receiver<Integrated>,
    /// Tasks being merged into the integration branch.
    integrating: HashSet<(String, String)>,
    integration_check_tx: crate::host::Sender<CheckedIntegration>,
    integration_check_rx: Receiver<CheckedIntegration>,
    integration_checks: HashSet<(String, String)>,
    integration_checked: HashMap<(String, String), (u64, String, Task)>,
    integration_dirty: Arc<AtomicBool>,
    integration_revision: u64,
    worktrees: Vec<WorktreeEntry>,
    worktrees_at: Option<Instant>,
    focus_terminal: Option<String>,
    last_layout: Option<Layout>,
    watcher: Option<Debouncer<RecommendedWatcher>>,
    /// Worktrees being watched, each with its path as the file system reports it.
    watched: Arc<Mutex<Vec<(PathBuf, PathBuf)>>>,
    watch_tx: crate::host::Sender<PathBuf>,
    watch_rx: Receiver<PathBuf>,
    toast: Option<(String, Instant)>,
    pending_open: Vec<(String, PathBuf)>,
    resume: Vec<SessionRecord>,
    restarting: bool,
    restart_event: Option<serde_json::Value>,
    restart_layout: Option<imacs_core::store::RestartState>,
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
    project_picker: Option<PathBuf>,
    approval_decisions: HashSet<(String, String)>,
    focus_terminal: Option<String>,
    cleanup_open: bool,
    cleanup_selected: HashSet<PathBuf>,
    /// Orphaned sandboxes picked for removal in the clean-up dialog.
    cleanup_sandboxes: HashSet<String>,
    cleanup_delete_branches: bool,
    plan_cleanup_open: bool,
    plan_cleanup_selected: HashSet<(PathBuf, String)>,
    diff_views: HashMap<PathBuf, DiffState>,
    icon: egui::TextureHandle,
    load: (Instant, Option<f64>),
    workspace_input: String,
    explorers: HashMap<PathBuf, ExplorerState>,
    split: Option<f32>,
    diff_split: Option<DiffSplit>,
    editor_file: HashMap<PathBuf, String>,
    focus_editor: bool,
    editor_id: Option<egui::Id>,
    terminal_views: HashMap<egui::Id, TerminalView>,
    expanded_tasks: HashSet<(String, String)>,
    selected: HashSet<String>,
    steer_text: String,
    steer_target: SteerTarget,
    focus_steer: bool,
    confirm_close: Option<String>,
    confirm_plan_close: Option<(String, PlanStatus)>,
    new_dialog: Option<NewDialog>,
    quick_open: Option<QuickOpen>,
    voice: Voice,
    /// The proposal on screen and when it appeared, which enables Apply after a moment.
    config_seen: Option<(String, Instant)>,
    /// The user's note on the proposal on screen, sent with Apply or Reject.
    config_note: String,
    /// The theme picker saves to this machine's config.d file instead of config.toml.
    theme_host_only: bool,
    /// Open or close the Settings menu on the next frame.
    show_settings: Option<bool>,
    voice_tx: Sender<VoiceEvent>,
    voice_rx: Receiver<VoiceEvent>,
    screenshot: Option<Screenshot>,
}

impl Client {
    fn new(ctx: &egui::Context, options: &mut Options, icon: &egui::IconData) -> Self {
        let (voice_tx, voice_rx) = std::sync::mpsc::channel();
        Self {
            project_picker: None,
            approval_decisions: HashSet::new(),
            focus_terminal: None,
            cleanup_open: false,
            cleanup_selected: HashSet::new(),
            cleanup_sandboxes: HashSet::new(),
            cleanup_delete_branches: true,
            plan_cleanup_open: false,
            plan_cleanup_selected: HashSet::new(),
            diff_views: HashMap::new(),
            icon: ctx.load_texture("atomisk", egui::ColorImage::from(icon), egui::TextureOptions::LINEAR),
            load: (Instant::now() - Duration::from_secs(10), None),
            workspace_input: String::new(),
            explorers: HashMap::new(),
            split: None,
            diff_split: None,
            editor_file: HashMap::new(),
            focus_editor: false,
            editor_id: None,
            terminal_views: HashMap::new(),
            expanded_tasks: HashSet::new(),
            selected: HashSet::new(),
            steer_text: String::new(),
            steer_target: SteerTarget::Active,
            focus_steer: false,
            confirm_close: None,
            confirm_plan_close: None,
            new_dialog: None,
            quick_open: None,
            voice: Voice::Idle,
            config_seen: None,
            config_note: String::new(),
            theme_host_only: false,
            show_settings: None,
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

/// A short-lived desktop borrow. Runtime effects are sent as host commands.
struct Desktop<'a> {
    runtime: &'a mut State,
    client: &'a mut Client,
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
    replies: Vec<Receiver<ControlOutcome>>,
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
            self.replies.push(self.host.command(command));
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
        let client = Client::new(&cc.egui_ctx, &mut options, icon);
        let state = State::new(cc, options, icon);
        Self { host: state.into_host(), client, replies: Vec::new() }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut state = self.host.state();
        self.replies.retain(|reply| match reply.try_recv() {
            Ok(ControlOutcome::Done(response)) => {
                if let Some(error) = response.error {
                    state.toast(error);
                }
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

impl State {
    fn into_host(self) -> crate::host::Host {
        let (wake, ctx, commands) = (self.wake.clone(), self.ctx.clone(), self.control_tx.clone());
        crate::host::Host::start(self, wake, ctx, commands)
    }

    pub fn new(cc: &eframe::CreationContext<'_>, options: Options, _icon: &egui::IconData) -> State {
        std::thread::spawn(|| LazyLock::force(&SHELL_PATH));
        // Before any git runs in a folder a sandbox has worked in. Records of sandboxes that are gone go after.
        crate::sandbox::load_pins(&paths::git_pins_dir());
        let sbx = if cfg!(test) { "/nonexistent/imacs-test-sbx" } else { "sbx" };
        let config_path = paths::config_file();
        let (config, config_error) = match Config::load_or_create(&config_path) {
            Ok(config) => (config, None),
            Err(err) => (Config::default(), Some(format!("{err:#}"))),
        };
        let ctx = cc.egui_ctx.clone();
        let (palette, theme_errors) = Palette::load(&config.theme, &theme::themes_dir(&config_path));
        palette.apply(&ctx);
        let theme_error = theme_error(&theme_errors);

        let hub = Arc::new(Mutex::new(Hub::default()));
        let wake = crate::host::Wake::default();
        let (hook_tx, hook_rx) = wake.channel();
        let (control_tx, control_rx) = wake.channel();
        let (action_tx, action_rx) = wake.channel();
        let (usage, usage_tx) = crate::usage::UsagePanel::new(ctx.clone());
        let sandbox_pull = crate::usage::SandboxPull::new(usage_tx.clone());
        let governor = Arc::new(Governor::new(config.resources.clone(), Some(ctx.clone())));
        let channels =
            Channels { hooks: hook_tx, control: control_tx.clone(), usage: usage_tx, governor: governor.clone() };
        let hook_error =
            ipc::serve(&paths::socket_path(), hub.clone(), channels, ctx.clone()).err().map(|e| format!("{e:#}"));
        // Only the instance that owns the socket owns the pool, so a second IMACS never replaces it.
        let mut notices = Vec::new();
        let jobserver = match hook_error {
            Some(_) => None,
            None => {
                // Only the instance that owns the socket records its runs, so second instances stay out of the log.
                notices.extend(crate::crash::started(&paths::imacs_home()));
                let tokens = config.resources.jobs(governor.cores);
                match (tokens > 0).then(|| Jobserver::create(&Jobserver::path_for(&paths::imacs_home()), tokens)) {
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

        let (mut plans, plan_error) = match plan::load_plans(&paths::imacs_home()) {
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
        let mut app = State {
            ctx: ctx.clone(),
            wake: wake.clone(),
            control_tx,
            client_commands: Vec::new(),
            client_events: Vec::new(),
            action_tx,
            action_rx,
            highlighter: Arc::new(Highlighter::new(&palette.syntax)),
            palette: Arc::new(palette),
            config_stamp: config_fingerprints(&config_path),
            repo_workflows: HashMap::new(),
            project_roots: HashMap::new(),
            config,
            config_path,
            config_checked: Instant::now(),
            config_error,
            theme_error,
            theme_choices: Arc::default(),
            host_theme: false,
            sessions: Vec::new(),
            codex_versions: HashMap::new(),
            pending_codex: Vec::new(),
            // Tests never reach the sbx installed on the machine running them.
            sbx: sbx.into(),
            sandbox_check: None,
            sandbox_lines: Vec::new(),
            pending_sandbox: Vec::new(),
            lifecycle_jobs: Vec::new(),
            startup_cleanup: None,
            shutting_down: false,
            ref_checks: crate::ref_guard::Checks::new(),
            sandbox_usage: Default::default(),
            active: None,
            diff_open: true,
            diff_root: None,
            diffs: HashMap::new(),
            worker: DiffWorker::new(wake),
            hub,
            hook_rx,
            usage,
            sandbox_pull,
            control_rx,
            hook_error,
            governor,
            jobserver,
            busy_checked: Instant::now(),
            output_delay: None,
            approvals_checked: Instant::now() - APPROVAL_CHECK_INTERVAL,
            needs_input_checked: Instant::now() - NEEDS_INPUT_CHECK_INTERVAL,
            store: Store::load(&paths::store_file()),
            dismissed: HashSet::new(),
            repos: Vec::new(),
            open_projects: Vec::new(),
            workspace: options.workspace.clone(),
            worktree_project: HashMap::new(),
            title: String::new(),
            cleanup: None,
            plan_cleanup: None,
            panel: PanelMode::Diff,
            editors: HashMap::new(),
            events: Vec::new(),
            event_seq: 0,
            last_status: HashMap::new(),
            waiters: Vec::new(),
            heartbeat: heartbeat::Heartbeat::default(),
            tickets,
            plans,
            selected_plan,
            changed_plans: HashSet::new(),
            view: None,
            plan_view: false,
            chief_open: false,
            worktree_tx,
            worktree_rx,
            creating: Vec::new(),
            integrate_tx,
            integrate_rx,
            integrating: HashSet::new(),
            integration_check_tx,
            integration_check_rx,
            integration_checks: HashSet::new(),
            integration_checked: HashMap::new(),
            integration_dirty: Arc::new(AtomicBool::new(false)),
            integration_revision: 0,
            worktrees: Vec::new(),
            worktrees_at: None,
            focus_terminal: None,
            last_layout: None,
            watcher: None,
            watched: Arc::default(),
            watch_tx,
            watch_rx,
            toast: None,
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
        app.refresh_themes();
        app.open_initial_projects(options.projects, options.workspace);
        app.apply_config();
        if app.hook_error.is_none() {
            app.begin_startup_cleanup();
            app.restore_restart();
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
        self.toast = Some((text.into(), Instant::now()));
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
            }
        }
    }

    // ── drawing ───────────────────────────────────────────────────────────
}

impl State {
    fn begin_startup_cleanup(&mut self) {
        let (home, config, sbx, actions) =
            (paths::imacs_home(), self.config.clone(), self.sbx.clone(), self.action_tx.clone());
        let before = SystemTime::now();
        self.startup_cleanup = Some(std::thread::spawn(move || {
            let sbx = resolve_program(&sbx);
            let notes = match crate::lifecycle::sweep_before(&home, &config, &sbx, before) {
                Ok(notes) => notes,
                Err(err) => vec![format!("Startup cleanup skipped: {err:#}")],
            };
            crate::sandbox::prune_pins(&home, &home.join("git-pins"), &sbx);
            crate::lifecycle::record_leftovers(&home, &notes);
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
        crate::crash::exited(&paths::imacs_home());
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
        let mut pending = std::mem::take(&mut self.pending_sandbox);
        for round in 0..2 {
            let deadline = Instant::now() + QUIT_WAIT;
            pending.retain(|(launch, ready)| {
                let left = deadline.saturating_duration_since(Instant::now());
                if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = ready.recv_timeout(left) {
                    return true;
                }
                if let AgentRuntime::Sandbox(sandbox) = &launch.runtime {
                    names.insert(sandbox.name.clone());
                }
                false
            });
            // Also one a closed plan's cleanup waits for, which is not pending here.
            if round == 0 {
                crate::sandbox::stop_creating();
            }
        }
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
        // Work still running after that ends with IMACS, as in a crash; the next start's sweep finds
        // what it left.
        let deadline = Instant::now() + QUIT_JOBS_WAIT;
        let unfinished = self.lifecycle_jobs.drain(..).filter(|job| !job.wait_until(deadline)).count();
        if unfinished > 0 {
            notes.push(format!("{unfinished} sandbox or worktree cleanup jobs were still running at quit"));
        }
        crate::lifecycle::record_leftovers(&paths::imacs_home(), &notes);
        self.check_all_refs(&self.exempt_branches());
        self.ref_checks.finish();
        if let Some(job) = self.startup_cleanup.take() {
            let _ = job.join();
        }
        // A second instance never owned the socket; removing it would cut the first off.
        if self.hook_error.is_none() {
            let _ = std::fs::remove_file(paths::socket_path());
        }
    }
}

fn state_refresh(state: &mut DiffData) {
    state.dirty_since = Some(Instant::now() - Duration::from_secs(1));
}

fn status_color(status: AgentStatus, p: &Palette) -> Color32 {
    match status {
        AgentStatus::Starting => p.muted,
        AgentStatus::Idle => lerp(p.muted, p.foreground, 0.3),
        AgentStatus::Working => p.accent,
        AgentStatus::NeedsInput => p.warning,
        AgentStatus::Done => p.success,
        AgentStatus::Exited => lerp(p.error, p.background, 0.4),
    }
}

/// The status dot; working agents pulse like Claude Code's spinner.
fn status_dot(status: AgentStatus, p: &Palette, time: f64) -> Color32 {
    let color = status_color(status, p);
    if status == AgentStatus::Working {
        let pulse = 0.55 + 0.45 * ((time * 3.2).sin() * 0.5 + 0.5) as f32;
        lerp(p.background, color, pulse)
    } else {
        color
    }
}

fn task_result(
    ui: &mut egui::Ui,
    task: &Task,
    status: Option<AgentStatus>,
    expanded: bool,
    read_only: bool,
    config: &Config,
    p: &Palette,
) -> Option<&'static str> {
    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
        let mut action = None;
        if task.finished_at.is_some() {
            ui.label(
                RichText::new(task.completion_line().unwrap_or_else(|| "Agent completed".into()))
                    .color(p.success)
                    .size(12.0),
            );
        } else if matches!(status, Some(AgentStatus::Idle | AgentStatus::Done))
            && let Some(line) = task.reported_line(imacs_core::store::now())
        {
            ui.label(RichText::new(line).color(p.muted).size(12.0));
        }
        if let Some(report) = &task.report {
            let summary = if expanded {
                report.summary.clone()
            } else {
                truncate(&report.summary.lines().take(3).collect::<Vec<_>>().join("\n"), 360)
            };
            let (summary, color) = if task.finished_at.is_some() {
                (summary, p.foreground)
            } else {
                let color = match report.status.as_str() {
                    "done" => p.success,
                    "blocked" | "question" => p.warning,
                    _ => p.info,
                };
                (format!("{}: {summary}", report.status), color)
            };
            ui.label(RichText::new(summary).color(color).size(12.0));
        }
        if task.finished_at.is_some() {
            let available = task.worktree.as_ref().is_some_and(|path| path.is_dir());
            ui.label(RichText::new(format!("+{} −{}", task.additions, task.deletions)).color(p.muted).size(12.0));
            ui.horizontal(|ui| {
                if ui.add_enabled(available, egui::Button::new("diff").small()).clicked() {
                    action = Some("diff");
                }
                if !read_only
                    && ui
                        .add_enabled(
                            status.is_none()
                                && available
                                && (task.resume_id.is_some()
                                    || config.agent(&task.agent).is_some_and(|agent| agent.is_codex())),
                            egui::Button::new("resume").small(),
                        )
                        .clicked()
                {
                    action = Some("resume");
                }
            });
        }
        action
    })
    .inner
}

fn status_glyph(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::NeedsInput => "◆",
        AgentStatus::Done => "●",
        AgentStatus::Working => "◐",
        AgentStatus::Exited => "×",
        _ => "○",
    }
}

fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
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

fn open_path(path: &Path) -> std::io::Result<std::process::Child> {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    std::process::Command::new(opener).arg(path).spawn()
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
                ClientEvent::ConfigChanged => self.runtime.palette.apply(ctx),
                ClientEvent::Viewport(command) => ctx.send_viewport_cmd(command),
                ClientEvent::ShowSettings(open) => self.client.show_settings = Some(open),
                ClientEvent::VoiceFailed { text, error } => self.voice_failed(&text, &error),
                ClientEvent::Edit { root, path } => {
                    self.client.focus_editor = true;
                    if let Some(path) = path {
                        self.client.editor_file.insert(root, path);
                    }
                }
            }
        }
    }

    fn sync_diffs(&mut self) {
        for (root, data) in &self.runtime.diffs {
            let view = self.client.diff_views.entry(root.clone()).or_default();
            view.prepared = data.prepared.clone();
            view.error = data.error.clone();
            view.loading = data.loading;
            view.comments = data.comments.clone();
            if view.viewing == data.viewing {
                view.file = data.file.clone();
                view.file_error = data.file_error.clone();
            }
        }
    }

    /// Load per core, with the pool and queue on hover.
    fn load_gauge(&mut self, ui: &mut egui::Ui) {
        if self.client.load.0.elapsed() > Duration::from_secs(2) {
            self.client.load = (Instant::now(), crate::governor::load_average());
        }
        let Some(load) = self.client.load.1 else { return };
        let p = self.runtime.palette.clone();
        let per_core = load / self.runtime.governor.cores as f64;
        let color = if per_core >= 1.0 {
            p.error
        } else if per_core >= 0.7 {
            p.warning
        } else {
            p.muted
        };
        let (running, waiting) = self.runtime.governor.snapshot();
        let mut hover = format!("Load {load:.1} on {} cores", self.runtime.governor.cores);
        match &self.runtime.jobserver {
            Some(jobserver) => hover.push_str(&format!("\nCompilers of all agents share {} jobs", jobserver.tokens())),
            None => hover.push_str("\nNo shared compiler job pool"),
        }
        for class in [SlotClass::Test, SlotClass::Qa] {
            let busy = running.iter().filter(|t| t.class == class).count();
            let queued = waiting.iter().filter(|t| t.class == class).count();
            hover.push_str(&format!(
                "\n{} slots: {busy}/{} in use, {queued} waiting",
                class.name(),
                self.runtime.governor.slots(class)
            ));
        }
        let response = ui.label(RichText::new(format!("load {load:.1}")).color(color).size(12.0)).on_hover_text(hover);
        topbar::record_item(ui, "load", &response);
    }

    fn metrics(&self, ui: &egui::Ui) -> Metrics {
        let p = &self.runtime.palette;
        let font = p.mono(p.fonts.mono_size.unwrap_or(self.runtime.config.font.size));
        let cell = ui.fonts_mut(|f| vec2(f.glyph_width(&font, 'M'), f.row_height(&font)));
        Metrics { font, cell: vec2(cell.x, cell.y.ceil()) }
    }

    fn steer_bar(&mut self, ui: &mut egui::Ui) {
        let p = self.runtime.palette.clone();
        ui.horizontal(|ui| {
            let active_title = self
                .runtime
                .active_index()
                .map(|i| self.runtime.sessions[i].title.clone())
                .unwrap_or_else(|| "none".into());
            let agents = self.runtime.sessions.iter().filter(|s| s.is_agent() && s.can_receive()).count();
            let selected = self.client.selected.len();
            let label = match self.client.steer_target {
                SteerTarget::Active => format!("→ {active_title}"),
                SteerTarget::Agents => format!("→ all agents ({agents})"),
                SteerTarget::Selected => format!("→ selected ({selected})"),
            };
            egui::ComboBox::from_id_salt("steer-target").selected_text(label).width(190.0).show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut self.client.steer_target,
                    SteerTarget::Active,
                    format!("Active: {active_title}"),
                );
                ui.selectable_value(
                    &mut self.client.steer_target,
                    SteerTarget::Agents,
                    format!("All agents ({agents})"),
                );
                ui.selectable_value(
                    &mut self.client.steer_target,
                    SteerTarget::Selected,
                    format!("Selected ({selected})"),
                );
            });

            let id = ui.scope_id().with("steer-input");
            let focused = ui.memory(|m| m.has_focus(id));
            let mut send = false;
            if focused && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter)) {
                send = true;
            }
            let width = ui.available_width() - 70.0;
            let edit = egui::TextEdit::multiline(&mut self.client.steer_text)
                .id(id)
                .desired_rows(1)
                .desired_width(width)
                .hint_text(format!(
                    "Steer · Enter sends · Shift+Enter newline · {} focuses",
                    shortcut_label(ui.ctx(), &self.runtime.config.keys.focus_steer)
                ))
                .font(p.ui(13.5));
            let response = ui.add(edit);
            if std::mem::take(&mut self.client.focus_steer) {
                response.request_focus();
            }
            let button = egui::Button::new(RichText::new("Send").color(p.on_accent)).fill(p.accent);
            if ui.add(button).clicked() {
                send = true;
            }
            if send {
                self.send_steer();
            }
        });
    }

    fn welcome(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let p = self.runtime.palette.clone();
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("welcome")
                .max_rect(Rect::from_center_size(rect.center(), vec2(rect.width().min(460.0), 260.0))),
        );
        child.vertical_centered(|ui| {
            ui.label(RichText::new("IMACS").size(28.0).strong().color(p.accent));
            ui.label(RichText::new("Emacs, but for agents").color(p.muted));
            ui.label(RichText::new("an atomisk.ai project").color(p.muted).size(12.0));
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                for agent in self.runtime.config.agents.clone() {
                    if ui.button(format!("Start {}", agent.name)).clicked() {
                        self.runtime.client_commands.push(ControlRequest::Start {
                            agent: agent.name.clone(), folder: Some(self.runtime.launch_dir.to_string_lossy().into_owned()),
                            repo: None, branch: None, prompt: None, args: Vec::new(),
                        });
                    }
                }
            });
            ui.add_space(6.0);
            ui.label(RichText::new(format!("in {}", paths::display(&self.runtime.launch_dir))).color(p.muted).size(12.0));
            ui.add_space(14.0);
            ui.label(RichText::new("Click an agent's title to see its worktree's diff against main.\nComment on any line and send the review straight to the agent.").color(p.muted).size(12.0));
        });
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
        let p = self.runtime.palette.clone();

        egui::Panel::top("top")
            .exact_size(34.0)
            .frame(egui::Frame::new().fill(p.topbar).inner_margin(egui::Margin::symmetric(10, 4)))
            .show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("steer")
            .frame(egui::Frame::new().fill(p.panel).inner_margin(egui::Margin::symmetric(10, 7)))
            .show(ui, |ui| self.steer_bar(ui));

        let area = ui.available_rect_before_wrap();
        let history_board = self.runtime.plan_view
            && self.runtime.panel == PanelMode::Plan
            && self.runtime.plans[self.runtime.selected_plan].status != PlanStatus::Open;
        let diff_open = !history_board
            && self.runtime.diff_open
            && (self.runtime.diff_target().is_some() || self.runtime.panel == PanelMode::Plan);
        let layout_config =
            layout::LayoutConfig { diff_width: self.client.split, ..self.runtime.config.layout.clone() };
        let visible = if history_board { Vec::new() } else { self.runtime.visible_sessions() };
        let completed = if history_board { 0 } else { self.runtime.completed_tasks().len() };
        let dock_width = if self.runtime.chief_open && self.runtime.chief_dock() == ChiefDock::Docked {
            self.runtime.store.chief_width.filter(|w| w.is_finite()).unwrap_or_else(|| {
                let sidebar =
                    if area.width() >= layout_config.sidebar_min_window { layout_config.sidebar_width } else { 0.0 };
                ((area.width() - sidebar) * 0.34).max(layout::MIN_DOCK_WIDTH)
            })
        } else {
            0.0
        };
        let mut layout = layout::compute(
            area.width(),
            area.height(),
            visible.len(),
            completed,
            diff_open,
            dock_width,
            &layout_config,
        );
        if layout.tabs {
            layout = layout::compute(
                area.width(),
                area.height() - TAB_HEIGHT,
                visible.len(),
                completed,
                diff_open,
                dock_width,
                &layout_config,
            );
            layout.tabs = visible.len() > layout.tiled || layout.tiled == 0;
        }

        self.runtime.last_layout = Some(layout);
        if layout.sidebar {
            egui::Panel::left("sidebar")
                .exact_size(self.runtime.config.layout.sidebar_width)
                .resizable(false)
                .frame(egui::Frame::new().fill(p.sidebar).inner_margin(egui::Margin::symmetric(10, 4)))
                .show(ui, |ui| self.sidebar(ui));
        }

        let chief_rect = if layout.dock_width > 0.0 {
            Some(
                egui::Panel::left("chief-dock")
                    .exact_size(layout.dock_width)
                    .resizable(false)
                    .frame(egui::Frame::new().fill(p.terminal).inner_margin(egui::Margin::same(6)))
                    .show(ui, |ui| self.chief_content(ui))
                    .response
                    .rect,
            )
        } else {
            None
        };

        egui::CentralPanel::default().frame(egui::Frame::new().fill(p.background)).show(ui, |ui| {
            let mut rect = ui.max_rect();
            if history_board {
                self.plan_panel(ui, rect);
                return;
            }
            let statuses: Vec<AgentStatus> = visible.iter().map(|&i| self.runtime.sessions[i].status).collect();
            let active = self.runtime.active_index().and_then(|a| visible.iter().position(|&i| i == a));
            let tiled: Vec<usize> =
                pick_tiled(&statuses, active, layout.tiled).into_iter().map(|k| visible[k]).collect();
            if layout.tabs {
                self.tab_strip(ui, rect.with_max_y(rect.top() + TAB_HEIGHT), &tiled, &visible);
                rect.min.y += TAB_HEIGHT;
            }
            if visible.is_empty() && completed == 0 && !diff_open {
                self.welcome(ui, rect);
                return;
            }
            let (diff_rect, term_rect) = match layout.diff {
                DiffPlacement::Right { width } => {
                    (Some(rect.with_min_x(rect.right() - width)), Some(rect.with_max_x(rect.right() - width)))
                }
                DiffPlacement::Top { height } => {
                    (Some(rect.with_max_y(rect.top() + height)), Some(rect.with_min_y(rect.top() + height)))
                }
                DiffPlacement::Full => (Some(rect), None),
                DiffPlacement::Hidden => (None, Some(rect)),
            };
            if let Some(term_rect) = term_rect {
                if visible.is_empty() && layout.completed == 0 {
                    self.welcome(ui, term_rect);
                } else {
                    self.tiles(ui, term_rect, &layout, &tiled);
                }
            }
            if let Some(diff_rect) = diff_rect {
                self.diff_panel(ui, diff_rect);
                if let DiffPlacement::Right { width } = layout.diff {
                    // Drag the divider to give the diff/editor more or less room; double-click resets.
                    let handle = Rect::from_center_size(diff_rect.left_center(), vec2(8.0, diff_rect.height()));
                    let response = ui.interact(handle, ui.scope_id().with("divider"), Sense::click_and_drag());
                    if response.hovered() || response.dragged() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                        ui.painter().rect_filled(handle.shrink2(vec2(3.0, 0.0)), 0.0, p.accent.gamma_multiply(0.6));
                    }
                    if response.dragged() {
                        self.client.split = Some(width - response.drag_delta().x);
                    }
                    if response.double_clicked() {
                        self.client.split = None;
                    }
                }
                let edge = match layout.diff {
                    DiffPlacement::Right { .. } => Some([diff_rect.left_top(), diff_rect.left_bottom()]),
                    DiffPlacement::Top { .. } => Some([diff_rect.left_bottom(), diff_rect.right_bottom()]),
                    _ => None,
                };
                if let Some(edge) = edge {
                    ui.painter().line_segment(edge, Stroke::new(1.0, p.separator));
                }
            }
        });

        if let Some(rect) = chief_rect
            && chief_divider(ui, rect, &mut self.runtime.store.chief_width, &p)
        {
            self.save_chief_preferences();
        }

        self.chief_ui(&ctx);
        self.runtime.desktop_split = self.client.split;
        self.runtime.desktop_diff_split = self.client.diff_split;
        self.dialogs(&ctx);
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod board_tests;

#[cfg(test)]
mod host_tests;

#[cfg(test)]
mod sandbox_tests;
