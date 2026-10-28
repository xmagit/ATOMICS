use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use egui::{Color32, FontId, Key, Pos2, Rect, RichText, Sense, Stroke, Vec2, pos2, vec2};
use imacs_core::config::{AgentKind, ChiefDock, Config, VoiceTarget};
use imacs_core::git::{self, Integration, Merge, Worktree, WorktreeEntry};
use imacs_core::layout::{self, DiffPlacement, DiffSplit, Layout};
use imacs_core::session::{AgentStatus, pick_tiled, title_for};
use imacs_core::store::{SessionRecord, Store};
use imacs_core::{paths, steer};
use notify_debouncer_mini::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer};

use crate::diff_view::{self, DiffAction, DiffState, DiffWorker, ExplorerState, Header, WorkerResult};
use crate::highlight::Highlighter;
use crate::ipc::{self, Channels, ControlMessage, ControlOutcome, HookMessage, Hub};
use crate::setup::{SHELL_PATH, env_var, global_hooks_installed, install_global_hooks, resolve_program};
use crate::terminal::{Metrics, Terminal};
use crate::theme::{Palette, lerp, shortcut};
use crate::voice::{MAX_RECORDING, Recorder};
use imacs_core::approvals::{ApprovalRequest, Decision};
use imacs_core::control::{ControlRequest, ControlResponse};
use imacs_core::hook::HookReply;
use imacs_core::plan::{self, Plan, PlanStatus, Report, Task, TaskStatus, short_slug};
use imacs_core::workspace::{self, Workspace};
use serde_json::json;

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
    pub title: String,
    pub status: AgentStatus,
    pub last_prompt: Option<String>,
    pub last_message: Option<String>,
    hooks_active: bool,
    approvals: Vec<PendingApproval>,
    allowed: Vec<ApprovalRequest>,
    held: Vec<String>,
    approval_hold: bool,
    menu_absent_since: Option<Instant>,
    pub selected: bool,
    /// The chief agent stays outside the agent fleet.
    pub chief: bool,
    /// A message typed into the terminal that the agent has not visibly picked up yet.
    pending: Option<Pending>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    Sent,
    Queued,
    Held,
    Unavailable,
}

struct PendingApproval {
    request: ApprovalRequest,
    reply: Option<Arc<ipc::ApprovalReply>>,
}

struct AgentLaunch {
    id: String,
    agent: imacs_core::config::AgentConfig,
    cwd: PathBuf,
    resume: Option<String>,
    extra_args: Vec<String>,
    prompt: Option<String>,
    chief: bool,
}

struct Pending {
    text: String,
    sent: Instant,
    attempts: u8,
    was_working: bool,
}

const APPROVAL_CHECK_INTERVAL: Duration = Duration::from_millis(250);
const APPROVAL_MENU_GRACE: Duration = Duration::from_secs(1);

const DELIVERY_TIMEOUT: Duration = Duration::from_secs(4);
const DELIVERY_ATTEMPTS: u8 = 3;

impl Session {
    fn starting(id: String, agent: &imacs_core::config::AgentConfig, cwd: PathBuf, chief: bool) -> Self {
        let worktree = git::worktree_of(&cwd);
        let title =
            if chief { "chief".into() } else { title_for(worktree.as_ref().and_then(|w| w.branch.as_deref()), &cwd) };
        Self {
            id,
            agent: agent.name.clone(),
            kind: agent.kind,
            terminal: None,
            claude_id: None,
            cwd,
            worktree,
            title,
            status: if agent.kind == AgentKind::Shell { AgentStatus::Idle } else { AgentStatus::Starting },
            last_prompt: None,
            last_message: None,
            hooks_active: false,
            approvals: Vec::new(),
            allowed: Vec::new(),
            held: Vec::new(),
            approval_hold: false,
            menu_absent_since: None,
            selected: false,
            chief,
            pending: None,
        }
    }

    fn root(&self) -> PathBuf {
        self.worktree.as_ref().map(|w| w.root.clone()).unwrap_or_else(|| self.cwd.clone())
    }

    fn branch(&self) -> Option<&str> {
        self.worktree.as_ref().and_then(|w| w.branch.as_deref())
    }

    fn is_agent(&self) -> bool {
        self.kind != AgentKind::Shell
    }

    fn can_receive(&self) -> bool {
        match &self.terminal {
            Some(t) => !t.exited(),
            None => self.claude_id.is_some() && self.status != AgentStatus::Exited,
        }
    }
}

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

/// The worktree clean-up dialog for one project.
struct Cleanup {
    repo: PathBuf,
    rows: Option<Vec<CleanupRow>>,
    preselect: Option<PathBuf>,
    delete_branches: bool,
    busy: Option<String>,
    log: Vec<String>,
    tx: Sender<CleanupEvent>,
    rx: Receiver<CleanupEvent>,
}

struct CleanupRow {
    health: git::WorktreeHealth,
    selected: bool,
}

enum CleanupEvent {
    Loaded(Result<Vec<git::WorktreeHealth>, String>),
    Removed(PathBuf, Result<(), String>),
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
}

impl StatusEvent {
    /// What `wait` matches on: the agent's status, or "reported".
    fn kind(&self) -> &str {
        if self.report.is_some() { "reported" } else { self.status.label() }
    }

    fn to_json(&self) -> serde_json::Value {
        let mut value = json!({ "seq": self.seq, "session": self.session, "title": self.title, "status": self.kind(), "at": self.at });
        value["plan"] = json!(self.plan);
        value["task"] = json!(self.task);
        if let Some((status, summary, task)) = &self.report {
            value["report"] = json!({ "status": status, "summary": summary, "task": task });
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

pub struct App {
    ctx: egui::Context,
    icon: egui::TextureHandle,
    config: Config,
    config_path: PathBuf,
    config_stamp: Option<SystemTime>,
    config_checked: Instant,
    config_error: Option<String>,
    palette: Arc<Palette>,
    highlighter: Arc<Highlighter>,
    sessions: Vec<Session>,
    codex_versions: HashMap<String, Arc<OnceLock<bool>>>,
    pending_codex: Vec<(AgentLaunch, Arc<OnceLock<bool>>)>,
    active: Option<String>,
    diff_open: bool,
    /// Worktree picked in the sidebar without an agent; the diff otherwise follows the active session.
    diff_root: Option<PathBuf>,
    diffs: HashMap<PathBuf, DiffState>,
    worker: DiffWorker,
    hub: Arc<Mutex<Hub>>,
    hook_rx: Receiver<HookMessage>,
    usage: crate::usage::UsagePanel,
    control_rx: Receiver<ControlMessage>,
    hook_error: Option<String>,
    busy_checked: Instant,
    approvals_checked: Instant,
    store: Store,
    dismissed: HashSet<String>,
    /// Main checkouts of the open projects (plus `repos` from the config).
    repos: Vec<PathBuf>,
    open_projects: Vec<PathBuf>,
    workspace: Option<String>,
    workspace_input: String,
    explorers: HashMap<PathBuf, ExplorerState>,
    /// Worktree folder → its project's main checkout.
    worktree_project: HashMap<PathBuf, PathBuf>,
    title: String,
    /// Diff/editor width set by dragging the divider.
    split: Option<f32>,
    /// Side by side or unified, picked in the diff header; wins over the config until IMACS quits.
    diff_split: Option<DiffSplit>,
    editor_file: HashMap<PathBuf, String>,
    cleanup: Option<Cleanup>,
    panel: PanelMode,
    editors: HashMap<PathBuf, Terminal>,
    focus_editor: bool,
    editor_id: Option<egui::Id>,
    events: Vec<StatusEvent>,
    event_seq: u64,
    last_status: HashMap<String, AgentStatus>,
    waiters: Vec<Waiter>,
    plans: Vec<Plan>,
    selected_plan: usize,
    changed_plans: HashSet<String>,
    expanded_tasks: HashSet<(String, String)>,
    /// The project tab on screen; `None` shows every project.
    view: Option<PathBuf>,
    plan_view: bool,
    /// The chief is shown, either floating or docked (otherwise only the pill).
    chief_open: bool,
    worktree_tx: Sender<(String, Result<PathBuf, String>, Launch)>,
    worktree_rx: Receiver<(String, Result<PathBuf, String>, Launch)>,
    creating: Vec<String>,
    integrate_tx: Sender<Integrated>,
    integrate_rx: Receiver<Integrated>,
    /// Tasks being merged into the integration branch.
    integrating: HashSet<(String, String)>,
    worktrees: Vec<WorktreeEntry>,
    worktrees_at: Option<Instant>,
    steer_text: String,
    steer_target: SteerTarget,
    focus_steer: bool,
    focus_terminal: Option<String>,
    confirm_close: Option<String>,
    new_dialog: Option<NewDialog>,
    quick_open: Option<QuickOpen>,
    last_layout: Option<Layout>,
    watcher: Option<Debouncer<RecommendedWatcher>>,
    /// Worktrees being watched, each with its path as the file system reports it.
    watched: Arc<Mutex<Vec<(PathBuf, PathBuf)>>>,
    watch_tx: Sender<PathBuf>,
    watch_rx: Receiver<PathBuf>,
    toast: Option<(String, Instant)>,
    voice: Voice,
    voice_tx: Sender<VoiceEvent>,
    voice_rx: Receiver<VoiceEvent>,
    screenshot: Option<Screenshot>,
    pending_open: Vec<(String, PathBuf)>,
    resume: Vec<SessionRecord>,
    launch_dir: PathBuf,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, options: Options, icon: &egui::IconData) -> App {
        std::thread::spawn(|| LazyLock::force(&SHELL_PATH));
        let config_path = paths::config_file();
        let (config, config_error) = match Config::load_or_create(&config_path) {
            Ok(config) => (config, None),
            Err(err) => (Config::default(), Some(format!("{err:#}"))),
        };
        let ctx = cc.egui_ctx.clone();
        apply_style(&ctx, &config);

        let hub = Arc::new(Mutex::new(Hub::default()));
        let (hook_tx, hook_rx) = channel();
        let (control_tx, control_rx) = channel();
        let (usage, usage_tx) = crate::usage::UsagePanel::new(ctx.clone());
        let channels = Channels { hooks: hook_tx, control: control_tx, usage: usage_tx };
        let hook_error =
            ipc::serve(&paths::socket_path(), hub.clone(), channels, ctx.clone()).err().map(|e| format!("{e:#}"));
        let (watch_tx, watch_rx) = channel();
        let (worktree_tx, worktree_rx) = channel();
        let (integrate_tx, integrate_rx) = channel();
        let (voice_tx, voice_rx) = channel();
        let launch_dir = std::env::current_dir().unwrap_or_else(|_| paths::home());

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
        let mut app = App {
            ctx: ctx.clone(),
            icon: ctx.load_texture("atomisk", egui::ColorImage::from(icon), egui::TextureOptions::LINEAR),
            palette: Arc::new(Palette::from_config(&config.theme)),
            highlighter: Arc::new(Highlighter::new(&Palette::from_config(&config.theme).syntax)),
            config_stamp: modified(&config_path),
            config,
            config_path,
            config_checked: Instant::now(),
            config_error,
            sessions: Vec::new(),
            codex_versions: HashMap::new(),
            pending_codex: Vec::new(),
            active: None,
            diff_open: true,
            diff_root: None,
            diffs: HashMap::new(),
            worker: DiffWorker::new(ctx.clone()),
            hub,
            hook_rx,
            usage,
            control_rx,
            hook_error,
            busy_checked: Instant::now(),
            approvals_checked: Instant::now() - APPROVAL_CHECK_INTERVAL,
            store: Store::load(&paths::store_file()),
            dismissed: HashSet::new(),
            repos: Vec::new(),
            open_projects: Vec::new(),
            workspace: options.workspace.clone(),
            workspace_input: String::new(),
            explorers: HashMap::new(),
            worktree_project: HashMap::new(),
            title: String::new(),
            split: None,
            diff_split: None,
            editor_file: HashMap::new(),
            cleanup: None,
            panel: PanelMode::Diff,
            editors: HashMap::new(),
            focus_editor: false,
            editor_id: None,
            events: Vec::new(),
            event_seq: 0,
            last_status: HashMap::new(),
            waiters: Vec::new(),
            plans,
            selected_plan,
            changed_plans: HashSet::new(),
            expanded_tasks: HashSet::new(),
            view: None,
            plan_view: false,
            chief_open: false,
            worktree_tx,
            worktree_rx,
            creating: Vec::new(),
            integrate_tx,
            integrate_rx,
            integrating: HashSet::new(),
            worktrees: Vec::new(),
            worktrees_at: None,
            steer_text: String::new(),
            steer_target: SteerTarget::Active,
            focus_steer: false,
            focus_terminal: None,
            confirm_close: None,
            new_dialog: None,
            quick_open: None,
            last_layout: None,
            watcher: None,
            watched: Arc::default(),
            watch_tx,
            watch_rx,
            toast: None,
            voice: Voice::Idle,
            voice_tx,
            voice_rx,
            screenshot: options.screenshot.map(|path| Screenshot {
                path,
                at: options.screenshot_delay as f64,
                requested: false,
                done: Arc::new(AtomicBool::new(false)),
            }),
            pending_open: options.open,
            resume: Vec::new(),
            launch_dir,
        };
        app.open_initial_projects(options.projects, options.workspace);
        app.apply_config();
        if !options.migration_errors.is_empty() {
            app.toast(options.migration_errors.join("\n"));
        }
        if let Some(error) = plan_error {
            app.toast(error);
        }
        app
    }

    /// `imacs <path>` or `--workspace`, else the repository IMACS was started in, else
    /// the projects from last time.
    fn open_initial_projects(&mut self, requested: Vec<PathBuf>, workspace: Option<String>) {
        let mut projects = requested;
        if let Some(name) = &workspace {
            match Workspace::load(name) {
                Ok(ws) => projects.extend(ws.projects),
                Err(err) => self.toast(format!("{err:#}")),
            }
        }
        if projects.is_empty() && workspace::project_root(&self.launch_dir).is_ok() {
            projects.push(self.launch_dir.clone());
        }
        if projects.is_empty() {
            projects = self.store.projects.clone();
        }
        for dir in projects {
            if let Ok(root) = workspace::project_root(&dir)
                && !self.open_projects.contains(&root)
            {
                self.open_projects.push(root);
            }
        }
        self.remember_projects();
    }

    fn remember_projects(&mut self) {
        for root in self.open_projects.clone() {
            self.store.touch_project(&root);
        }
        self.store.projects = self.open_projects.clone();
        let _ = self.store.save(&paths::store_file());
    }

    /// Checks out `branch` in a new worktree, from `start` (a plan's integration branch)
    /// or base_ref, and starts `launch` in it.
    fn create_worktree(&mut self, repo: PathBuf, branch: String, start: Option<String>, launch: Launch) {
        let path = git::worktree_path(&self.config.worktree_path, &repo, &branch);
        let (tx, base_ref, ctx) = (self.worktree_tx.clone(), self.config.base_ref.clone(), self.ctx.clone());
        self.creating.push(branch.clone());
        self.toast(format!("Creating worktree {}…", paths::display(&path)));
        std::thread::spawn(move || {
            // A retried task finds its worktree from the first attempt.
            let existing = git::worktree_of(&path).and_then(|w| w.branch).is_some_and(|b| b == branch);
            let result = if existing {
                Ok(path)
            } else {
                git::add_worktree_from(&repo, &path, &branch, start.as_deref(), &base_ref)
                    .map(|()| path)
                    .map_err(|e| format!("{e:#}"))
            };
            let _ = tx.send((branch, result, launch));
            ctx.request_repaint();
        });
    }

    fn open_cleanup(&mut self, repo: PathBuf, preselect: Option<PathBuf>) {
        let (tx, rx) = channel();
        let mut cleanup =
            Cleanup { repo, rows: None, preselect, delete_branches: true, busy: None, log: Vec::new(), tx, rx };
        self.load_cleanup(&mut cleanup);
        self.cleanup = Some(cleanup);
    }

    fn load_cleanup(&self, cleanup: &mut Cleanup) {
        cleanup.rows = None;
        let (tx, repo, base, ctx) =
            (cleanup.tx.clone(), cleanup.repo.clone(), self.config.base_ref.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let result = git::worktree_health(&repo, &base).map_err(|e| format!("{e:#}"));
            let _ = tx.send(CleanupEvent::Loaded(result));
            ctx.request_repaint();
        });
    }

    /// Runs a git maintenance command for the dialog in the background.
    fn cleanup_git(&self, cleanup: &mut Cleanup, label: &str, args: &'static [&'static str]) {
        cleanup.busy = Some(label.to_string());
        let (tx, repo, ctx, label) = (cleanup.tx.clone(), cleanup.repo.clone(), self.ctx.clone(), label.to_string());
        std::thread::spawn(move || {
            let message = match git::git(&repo, args) {
                Ok(_) => format!("{label}: done"),
                Err(err) => format!("{label}: {err:#}"),
            };
            let _ = tx.send(CleanupEvent::Finished(message));
            ctx.request_repaint();
        });
    }

    fn cleanup_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut cleanup) = self.cleanup.take() else { return };
        let mut reload = false;
        while let Ok(event) = cleanup.rx.try_recv() {
            match event {
                CleanupEvent::Loaded(Ok(list)) => {
                    let preselect = cleanup.preselect.take();
                    cleanup.rows = Some(
                        list.into_iter()
                            .map(|health| CleanupRow { selected: preselect.as_ref() == Some(&health.path), health })
                            .collect(),
                    );
                }
                CleanupEvent::Loaded(Err(err)) => {
                    cleanup.log.push(err);
                    cleanup.rows = Some(Vec::new());
                }
                CleanupEvent::Removed(path, result) => {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    match result {
                        Ok(()) => {
                            cleanup.log.push(format!("removed {name}"));
                            if let Some(rows) = &mut cleanup.rows {
                                rows.retain(|r| r.health.path != path);
                            }
                        }
                        Err(err) => cleanup.log.push(format!("kept {name}: {}", err.lines().last().unwrap_or(&err))),
                    }
                }
                CleanupEvent::Finished(message) => {
                    reload = !message.starts_with("removal");
                    cleanup.log.push(message);
                    cleanup.busy = None;
                    self.worktrees_at = None;
                }
            }
        }
        if reload {
            self.load_cleanup(&mut cleanup);
        }

        let p = self.palette.clone();
        let agent_roots: HashSet<PathBuf> = self.sessions.iter().map(Session::root).collect();
        let name = cleanup.repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut open = true;
        let mut remove = false;
        let mut action: Option<&str> = None;
        egui::Window::new(format!("Clean up worktrees · {name}"))
            .open(&mut open)
            .collapsible(false)
            .default_size(vec2(900.0, 620.0))
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let idle = cleanup.busy.is_none();
                    if ui
                        .add_enabled(idle, egui::Button::new("fetch --prune"))
                        .on_hover_text("Refresh which remote branches are gone (completed PRs)")
                        .clicked()
                    {
                        action = Some("fetch");
                    }
                    if ui
                        .add_enabled(idle, egui::Button::new("prune missing"))
                        .on_hover_text("Forget worktrees whose folder no longer exists")
                        .clicked()
                    {
                        action = Some("prune");
                    }
                    if ui.add_enabled(idle, egui::Button::new("reload")).clicked() {
                        action = Some("reload");
                    }
                    if let Some(busy) = &cleanup.busy {
                        ui.spinner();
                        ui.label(RichText::new(busy).color(p.muted));
                    }
                });
                ui.separator();
                let Some(rows) = &mut cleanup.rows else {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(
                            RichText::new("Checking every worktree for uncommitted work and merged branches…")
                                .color(p.muted),
                        );
                    });
                    return;
                };
                egui::ScrollArea::vertical().id_salt("cleanup-rows").max_height(440.0).show(ui, |ui| {
                    egui::Grid::new("cleanup-grid").num_columns(5).spacing([14.0, 6.0]).striped(true).show(ui, |ui| {
                        for row in rows.iter_mut() {
                            let h = &row.health;
                            let agent = agent_roots.contains(&h.path);
                            let blocked = agent || h.dirty_files > 0 || h.missing;
                            if blocked {
                                row.selected = false;
                            }
                            let why = if agent {
                                "an agent is working here"
                            } else if h.missing {
                                "folder is gone: use prune missing"
                            } else {
                                "has uncommitted changes"
                            };
                            ui.add_enabled(!blocked, egui::Checkbox::without_text(&mut row.selected))
                                .on_disabled_hover_text(why);
                            let folder =
                                h.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                            ui.label(RichText::new(folder).color(p.foreground));
                            let branch = h.branch.clone().unwrap_or_else(|| "detached".into());
                            ui.label(RichText::new(branch).color(p.muted).size(11.5));
                            ui.horizontal(|ui| {
                                let chip = |ui: &mut egui::Ui, text: String, color: Color32| {
                                    ui.label(RichText::new(text).color(color).size(11.0));
                                };
                                if h.missing {
                                    chip(ui, "missing".into(), p.error);
                                }
                                if agent {
                                    chip(ui, "● agent".into(), p.accent);
                                }
                                if h.dirty_files > 0 {
                                    chip(ui, format!("{} uncommitted", h.dirty_files), p.warning);
                                }
                                if h.merged {
                                    chip(ui, "merged".into(), p.success);
                                }
                                if h.upstream_gone {
                                    chip(ui, "remote gone".into(), p.success);
                                }
                                if !h.missing && h.dirty_files == 0 && !h.merged && !h.upstream_gone {
                                    chip(ui, "unmerged".into(), p.muted);
                                }
                            });
                            ui.label(
                                RichText::new(h.last_commit.clone().unwrap_or_default()).color(p.muted).size(11.0),
                            );
                            ui.end_row();
                        }
                    });
                });
                ui.separator();
                let selected = rows.iter().filter(|r| r.selected).count();
                ui.horizontal(|ui| {
                    if ui
                        .button("select safe")
                        .on_hover_text("Clean, and merged or with its remote branch deleted")
                        .clicked()
                    {
                        for row in rows.iter_mut() {
                            row.selected = row.health.is_safe_to_remove() && !agent_roots.contains(&row.health.path);
                        }
                    }
                    if ui.button("select none").clicked() {
                        for row in rows.iter_mut() {
                            row.selected = false;
                        }
                    }
                    ui.checkbox(&mut cleanup.delete_branches, "also delete their branches (only if merged)");
                    let label = format!("Remove {selected} worktree{}", if selected == 1 { "" } else { "s" });
                    let button = egui::Button::new(RichText::new(label).color(Color32::WHITE)).fill(if selected > 0 {
                        p.error.gamma_multiply(0.55)
                    } else {
                        p.raised
                    });
                    if ui.add_enabled(selected > 0 && cleanup.busy.is_none(), button).clicked() {
                        remove = true;
                    }
                });
                for line in cleanup.log.iter().rev().take(6) {
                    ui.label(RichText::new(line).color(p.muted).size(11.0));
                }
            });

        match action {
            Some("fetch") => self.cleanup_git(&mut cleanup, "fetch --prune", &["fetch", "--prune", "--quiet"]),
            Some("prune") => self.cleanup_git(&mut cleanup, "prune", &["worktree", "prune"]),
            Some("reload") => self.load_cleanup(&mut cleanup),
            _ => {}
        }
        if remove && let Some(rows) = &cleanup.rows {
            let targets: Vec<(PathBuf, Option<String>)> =
                rows.iter().filter(|r| r.selected).map(|r| (r.health.path.clone(), r.health.branch.clone())).collect();
            cleanup.busy = Some(format!("removing {} worktrees…", targets.len()));
            let (tx, repo, delete, ctx) =
                (cleanup.tx.clone(), cleanup.repo.clone(), cleanup.delete_branches, self.ctx.clone());
            std::thread::spawn(move || {
                for (path, branch) in targets {
                    let result =
                        git::remove_worktree(&repo, &path, branch.as_deref(), delete).map_err(|e| format!("{e:#}"));
                    let _ = tx.send(CleanupEvent::Removed(path, result));
                    ctx.request_repaint();
                }
                let _ = tx.send(CleanupEvent::Finished("removal finished".into()));
                ctx.request_repaint();
            });
        }
        if open {
            self.cleanup = Some(cleanup);
        } else {
            self.worktrees_at = None;
        }
    }

    fn open_project(&mut self, dir: &Path) {
        match workspace::project_root(dir) {
            Ok(root) => {
                if !self.open_projects.contains(&root) {
                    self.open_projects.push(root.clone());
                }
                self.remember_projects();
                self.apply_config();
                self.active = None;
                self.diff_root = Some(root.clone());
                self.diff_open = true;
                self.toast(format!("Opened {}", paths::display(&root)));
            }
            Err(err) => self.toast(format!("{err:#}")),
        }
    }

    fn close_project(&mut self, root: &Path) {
        self.open_projects.retain(|p| p != root);
        self.remember_projects();
        self.apply_config();
        if self.diff_root.as_deref().is_some_and(|d| self.project_of(d).as_deref() == Some(root) || d == root) {
            self.diff_root = None;
        }
    }

    fn pick_project(&mut self) {
        let start = self.repos.first().and_then(|r| r.parent()).map(Path::to_path_buf).unwrap_or_else(paths::home);
        if let Some(dir) = rfd::FileDialog::new().set_title("Open project").set_directory(start).pick_folder() {
            self.open_project(&dir);
        }
    }

    fn open_workspace(&mut self, name: &str) {
        match Workspace::load(name) {
            Ok(ws) => {
                self.open_projects = ws.projects.iter().filter_map(|p| workspace::project_root(p).ok()).collect();
                self.workspace = Some(name.to_string());
                self.remember_projects();
                self.apply_config();
                self.active = None;
                self.diff_root = self.open_projects.first().cloned();
                self.toast(format!("Workspace {name}: {} projects", self.open_projects.len()));
            }
            Err(err) => self.toast(format!("{err:#}")),
        }
    }

    fn save_workspace(&mut self, name: &str) {
        let ws = Workspace { name: name.trim().to_string(), projects: self.open_projects.clone() };
        match ws.save() {
            Ok(file) => {
                self.workspace = Some(ws.name);
                self.toast(format!("Saved {}", paths::display(&file)));
            }
            Err(err) => self.toast(format!("{err:#}")),
        }
    }

    /// The open project a folder belongs to.
    fn project_of(&self, dir: &Path) -> Option<PathBuf> {
        if let Some(root) = self.repos.iter().find(|r| dir.starts_with(r)) {
            return Some(root.clone());
        }
        self.worktree_project
            .iter()
            .filter(|(worktree, _)| dir.starts_with(worktree))
            .max_by_key(|(worktree, _)| worktree.as_os_str().len())
            .map(|(_, project)| project.clone())
    }

    fn apply_config(&mut self) {
        let mut hub = self.hub.lock().unwrap();
        hub.session_context = self.config.fill(&self.config.steer.session_context);
        hub.external_prefix = self.config.steer.external_prefix.clone();
        drop(hub);
        let mut repos = self.open_projects.clone();
        for repo in &self.config.repos {
            let root = workspace::project_root(&paths::expand(repo)).unwrap_or_else(|_| paths::expand(repo));
            if !repos.contains(&root) {
                repos.push(root);
            }
        }
        self.repos = repos;
        self.worktrees_at = None;
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

    // ── sessions ──────────────────────────────────────────────────────────

    fn spawn(&mut self, ctx: &egui::Context, agent_name: &str, cwd: PathBuf, resume: Option<String>) {
        if let Err(err) = self.start(ctx, agent_name, cwd, resume, Vec::new(), None) {
            self.toast(err);
        }
    }

    /// Starts an agent in a terminal and returns its session id.
    fn start(
        &mut self,
        ctx: &egui::Context,
        agent_name: &str,
        cwd: PathBuf,
        resume: Option<String>,
        extra_args: Vec<String>,
        prompt: Option<String>,
    ) -> Result<String, String> {
        self.start_as(ctx, agent_name, cwd, resume, extra_args, prompt, false)
    }

    /// Starts the chief agent in the main checkout of the project on screen.
    fn start_chief(&mut self, ctx: &egui::Context, prompt: Option<String>) -> Result<String, String> {
        let cwd = self.view.clone().or_else(|| self.repos.first().cloned()).unwrap_or_else(|| self.launch_dir.clone());
        let args = self.config.chief.args.clone();
        self.start_as(ctx, "chief", cwd, None, args, prompt, true)
    }

    #[allow(clippy::too_many_arguments)]
    fn start_as(
        &mut self,
        ctx: &egui::Context,
        agent_name: &str,
        cwd: PathBuf,
        resume: Option<String>,
        extra_args: Vec<String>,
        prompt: Option<String>,
        chief: bool,
    ) -> Result<String, String> {
        let agent = if chief {
            imacs_core::config::AgentConfig {
                name: "chief".into(),
                command: self.config.chief.command.clone(),
                kind: AgentKind::Claude,
                busy_text: None,
            }
        } else {
            let Some(agent) = self.config.agent(agent_name).cloned() else {
                return Err(format!("No agent named {agent_name} in the config"));
            };
            agent
        };
        if !cwd.is_dir() {
            return Err(format!("{} is not a folder", cwd.display()));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let codex = agent.kind == AgentKind::Generic
            && agent
                .command
                .first()
                .is_some_and(|program| Path::new(program).file_name().is_some_and(|name| name == "codex"));
        let launch = AgentLaunch { id: id.clone(), agent, cwd, resume, extra_args, prompt, chief };
        let version = if codex {
            let program = resolve_program(&launch.agent.command[0]);
            let ready = self.codex_version(ctx, program);
            match ready.get().copied() {
                Some(supported) => Some(supported),
                None => {
                    self.sessions.push(Session::starting(id.clone(), &launch.agent, launch.cwd.clone(), chief));
                    self.pending_codex.push((launch, ready));
                    self.activate(&id, false);
                    return Ok(id);
                }
            }
        } else {
            None
        };
        self.launch_agent(ctx, launch, version)
    }

    fn codex_version(&mut self, ctx: &egui::Context, program: String) -> Arc<OnceLock<bool>> {
        self.codex_versions
            .entry(program.clone())
            .or_insert_with(|| {
                let ready = Arc::new(OnceLock::new());
                let result = ready.clone();
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    let supported = crate::setup::codex_supports_hooks(&program, Duration::from_secs(3));
                    let _ = result.set(supported);
                    ctx.request_repaint();
                });
                ready
            })
            .clone()
    }

    fn launch_agent(
        &mut self,
        ctx: &egui::Context,
        launch: AgentLaunch,
        codex: Option<bool>,
    ) -> Result<String, String> {
        let AgentLaunch { id, agent, cwd, resume, extra_args, prompt, chief } = launch;
        let agent_name = agent.name.clone();
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let mut env: HashMap<String, String> =
            [("TERM", "xterm-256color".into()), ("COLORTERM", "truecolor".into()), ("TERM_PROGRAM", "imacs".into())]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();
        if let Some(path) = SHELL_PATH.as_ref() {
            env.insert("PATH".into(), path.clone());
        }

        let (program, mut args) = match agent.command.split_first() {
            Some((program, rest)) => (program.clone(), rest.to_vec()),
            None => (shell, vec!["-l".to_string()]),
        };
        let mut claude_id = None;
        // Every agent IMACS starts can report back to the plan with `imacs_report`.
        let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "imacs".into());
        let socket = paths::socket_path().to_string_lossy().into_owned();
        match agent.kind {
            AgentKind::Claude if chief => {
                // The chief gets every IMACS tool and knows how to run a plan.
                let config = json!({ "mcpServers": { "imacs": {
                    "command": exe, "args": ["mcp"], "env": { "IMACS_PANE": id, "IMACS_SOCKET": socket }
                } } });
                let projects: Vec<String> = self.repos.iter().map(|r| paths::display(r)).collect();
                let prompt = self
                    .config
                    .chief
                    .system_prompt
                    .replace("{projects}", &projects.join(", "))
                    .replace("{base_ref}", &self.config.base_ref);
                args.extend([
                    "--mcp-config".to_string(),
                    config.to_string(),
                    "--allowedTools".to_string(),
                    CHIEF_ALLOWED_TOOLS.join(","),
                    "--append-system-prompt".to_string(),
                    prompt,
                ]);
            }
            AgentKind::Claude => {
                let config = json!({ "mcpServers": { "imacs": {
                    "command": exe, "args": ["mcp", "--agent"], "env": { "IMACS_PANE": id, "IMACS_SOCKET": socket }
                } } });
                args.extend([
                    "--mcp-config".to_string(),
                    config.to_string(),
                    "--allowedTools".to_string(),
                    "mcp__imacs__imacs_report".to_string(),
                ]);
            }
            AgentKind::Generic if codex.is_some() => {
                let quoted = |text: &str| serde_json::to_string(text).unwrap_or_default();
                if codex == Some(true) {
                    if !args.iter().any(|arg| arg == "--no-daemon") {
                        args.push("--no-daemon".into());
                    }
                    args.extend(imacs_core::hook::codex_hook_args(Path::new(&exe), Path::new(&socket), &id));
                }
                args.extend([
                    "-c".to_string(),
                    format!("mcp_servers.imacs.command={}", quoted(&exe)),
                    "-c".to_string(),
                    "mcp_servers.imacs.args=[\"mcp\", \"--agent\"]".to_string(),
                    "-c".to_string(),
                    format!(
                        "mcp_servers.imacs.env={{ IMACS_PANE = {}, IMACS_SOCKET = {} }}",
                        quoted(&id),
                        quoted(&socket)
                    ),
                    "-c".to_string(),
                    "mcp_servers.imacs.default_tools_approval_mode=\"approve\"".to_string(),
                ]);
            }
            _ => {}
        }
        if agent.kind == AgentKind::Claude {
            args.push("--settings".into());
            args.push(imacs_core::hook::session_settings(Path::new(&exe), Path::new(&socket), &id, false).to_string());
            let session_id = match resume {
                Some(id) => {
                    args.extend(["--resume".to_string(), id.clone()]);
                    id
                }
                None => {
                    let id = uuid::Uuid::new_v4().to_string();
                    args.extend(["--session-id".to_string(), id.clone()]);
                    id
                }
            };
            claude_id = Some(session_id);
        }
        args.extend(extra_args);
        let prompt = match prompt {
            Some(text) if agent.kind != AgentKind::Claude && text.len() > LONG_PROMPT => Some(brief_file(&id, &text)?),
            other => other,
        };
        let shell_prompt = match (&prompt, agent.kind) {
            (Some(prompt), AgentKind::Shell) => Some(prompt.clone()),
            (Some(prompt), _) => {
                args.push(prompt.clone());
                None
            }
            _ => None,
        };
        let program = resolve_program(&program);
        match Terminal::spawn(ctx, self.palette.clone(), program, args, &cwd, env, self.config.terminal.scrollback) {
            Ok(terminal) => {
                let mut session = Session::starting(id.clone(), &agent, cwd, chief);
                session.terminal = Some(terminal);
                session.claude_id = claude_id;
                let starting = self.sessions.iter().position(|session| session.id == id);
                if let Some(index) = starting {
                    session.selected = self.sessions[index].selected;
                    self.sessions[index] = session;
                } else {
                    self.sessions.push(session);
                }
                if let Some(text) = shell_prompt
                    && let Some(terminal) = self.sessions.iter().find(|s| s.id == id).and_then(|s| s.terminal.as_ref())
                {
                    terminal.submit(&text);
                }
                if !chief && starting.is_none() {
                    self.activate(&id, false);
                }
                self.remember(&id);
                Ok(id)
            }
            Err(err) => Err(format!("Could not start {agent_name}: {err:#}")),
        }
    }

    /// Finds a session by id, id prefix, or title.
    fn find(&self, reference: &str) -> Option<usize> {
        let reference = reference.trim();
        self.sessions
            .iter()
            .position(|s| s.id == reference)
            .or_else(|| self.sessions.iter().position(|s| s.title.eq_ignore_ascii_case(reference)))
            .or_else(|| {
                let matches: Vec<usize> =
                    (0..self.sessions.len()).filter(|&i| self.sessions[i].id.starts_with(reference)).collect();
                (matches.len() == 1).then(|| matches[0])
            })
    }

    fn describe(&self, index: usize) -> serde_json::Value {
        let s = &self.sessions[index];
        let stats = self.diffs.get(&s.root()).and_then(|d| d.prepared.as_ref()).map(|p| {
            json!({
                "files": p.diff.files.len(), "additions": p.additions, "deletions": p.deletions
            })
        });
        json!({
            "id": s.id,
            "title": s.title,
            "agent": s.agent,
            "status": s.status.label(),
            "folder": s.cwd,
            "worktree": s.worktree.as_ref().map(|w| &w.root),
            "branch": s.branch(),
            "outside_imacs": s.terminal.is_none() && s.status != AgentStatus::Starting,
            "claude_session_id": s.claude_id,
            "last_prompt": s.last_prompt,
            "approval": s.approvals.first().map(|p| &p.request),
            "diff": stats,
        })
    }

    fn control(&mut self, ctx: &egui::Context, request: ControlRequest, reply: Sender<ControlOutcome>) {
        let done = |response: ControlResponse| ControlOutcome::Done(response);
        let missing = |reference: &str| done(ControlResponse::error(format!("no session matches {reference:?}")));
        let target = match &request {
            ControlRequest::PlanSet { plan: Some(id), .. } | ControlRequest::PlanClose { plan: id, .. } => {
                Some(Some(id.as_str()))
            }
            ControlRequest::PlanUpdate { plan, .. }
            | ControlRequest::PlanDispatch { plan, .. }
            | ControlRequest::PlanIntegrate { plan, .. } => Some(plan.as_deref()),
            _ => None,
        };
        let target = if let Some(id) = target {
            match plan::resolve_plan(&self.plans, id) {
                Ok(index) => index,
                Err(err) => {
                    let _ = reply.send(done(ControlResponse::error(err)));
                    return;
                }
            }
        } else {
            self.selected_plan
        };
        let outcome = match request {
            ControlRequest::Approve { session, decision, message } => match self.find(&session) {
                Some(index) => done(match self.approve(index, decision, message.as_deref()) {
                    Ok(()) => ControlResponse::ok(json!({"answered": true})),
                    Err(error) => ControlResponse::error(error),
                }),
                None => missing(&session),
            },
            ControlRequest::List => {
                let sessions: Vec<_> = (0..self.sessions.len()).map(|i| self.describe(i)).collect();
                let (window, monitor) = ctx.input(|i| (i.content_rect().size(), i.viewport().monitor_size));
                let layout = self.last_layout.map(|l| {
                    json!({ "sidebar": l.sidebar, "dock_width": l.dock_width, "diff": format!("{:?}", l.diff), "columns": l.columns, "rows": l.rows, "tiled": l.tiled, "tabs": l.tabs })
                });
                done(ControlResponse::ok(json!({
                    "active": self.active,
                    "plan_view": self.plan_view,
                    "visible": self.visible_sessions().iter().map(|&i| &self.sessions[i].id).collect::<Vec<_>>(),
                    "chief": { "open": self.chief_open, "dock": self.chief_dock(), "width": self.store.chief_width },
                    "window": [window.x, window.y],
                    "monitor": monitor.map(|m| [m.x, m.y]),
                    "layout": layout,
                    "sessions": sessions,
                })))
            }
            ControlRequest::Open { session, path } => match self.find(&session) {
                None => missing(&session),
                Some(index) => {
                    let (id, root) = (self.sessions[index].id.clone(), self.sessions[index].root());
                    self.activate(&id, true);
                    self.open_file(&root, &path);
                    done(ControlResponse::ok(json!({ "opened": path })))
                }
            },
            ControlRequest::Edit { session, path, line } => match self.find(&session) {
                None => missing(&session),
                Some(index) => {
                    let (id, root) = (self.sessions[index].id.clone(), self.sessions[index].root());
                    self.activate(&id, true);
                    self.edit(ctx, &root, Some(&path), line);
                    done(ControlResponse::ok(json!({ "editing": path, "line": line })))
                }
            },
            ControlRequest::Wait { sessions, statuses, after, timeout_secs } => {
                let mut ids = Vec::new();
                for reference in &sessions {
                    match self.find(reference) {
                        Some(index) => ids.push(self.sessions[index].id.clone()),
                        None => {
                            let _ = reply.send(missing(reference));
                            return;
                        }
                    }
                }
                let statuses = if statuses.is_empty() {
                    vec!["turn done".into(), "needs you".into(), "exited".into(), "reported".into()]
                } else {
                    statuses
                };
                let timeout = Duration::from_secs(timeout_secs.unwrap_or(120)).min(MAX_WAIT);
                self.waiters.push(Waiter {
                    sessions: ids,
                    statuses,
                    after: after.unwrap_or(self.event_seq),
                    deadline: Instant::now() + timeout,
                    reply,
                });
                self.serve_waiters();
                return;
            }
            ControlRequest::Result { session } => match self.find(&session) {
                None => missing(&session),
                Some(index) => {
                    let s = &self.sessions[index];
                    let screen = s.terminal.as_ref().map(|t| t.screen_text(40));
                    let mut result = self.describe(index);
                    result["last_message"] = json!(s.last_message);
                    result["screen"] = json!(screen);
                    done(ControlResponse::ok(result))
                }
            },
            ControlRequest::PlanSet { plan, goal, tasks, integration } => {
                let new = plan.is_none();
                let index = if plan.is_some() {
                    target
                } else {
                    let id = plan::unique_id(&goal, &self.plans, &paths::imacs_home());
                    self.plans.push(Plan { id, ..Plan::default() });
                    self.plans.len() - 1
                };
                self.plans[index].replace(goal, tasks);
                if let Some(branch) = integration.filter(|b| !b.trim().is_empty()) {
                    self.plans[index].integration = Some(branch.trim().to_string());
                }
                let project = self.plans[index]
                    .tasks
                    .iter()
                    .find_map(|t| t.repo.as_deref())
                    .map(paths::expand)
                    .or_else(|| self.plans[index].project.clone())
                    .or_else(|| {
                        self.diff_target().and_then(|t| self.project_of(&t)).or_else(|| self.repos.first().cloned())
                    });
                self.plans[index].project = project.and_then(|p| self.project_of(&p));
                self.save_plan(index);
                if new || (self.plan_view && self.selected_plan == index) {
                    self.selected_plan = index;
                    self.show_plan_fleet();
                    self.panel = PanelMode::Plan;
                    self.diff_open = true;
                } else {
                    self.changed_plans.insert(self.plans[index].id.clone());
                }
                let unknown = self.plans[index].unknown_dependencies();
                self.toast(if new {
                    "New plan: review and approve it on the plan board"
                } else {
                    "Plan updated: review and approve it on its board"
                });
                done(ControlResponse::ok(json!({
                    "plan": self.plans[index].id,
                    "tasks": self.plans[index].tasks.len(),
                    "approved": false,
                    "integration": self.plans[index].integration_branch(),
                    "unknown_dependencies": unknown,
                    "next": "The user reviews and approves the plan on the board; then call plan_dispatch.",
                })))
            }
            ControlRequest::PlanClose { plan: _, status } => match self.close_plan(target, status) {
                Ok(()) => done(ControlResponse::ok(self.plan_json(target))),
                Err(err) => done(ControlResponse::error(err)),
            },
            ControlRequest::PlanGet { plan: None } => done(ControlResponse::ok(json!({
                "plans": self.plans.iter().filter(|p| p.is_open()).map(Plan::summary).collect::<Vec<_>>()
            }))),
            ControlRequest::PlanGet { plan: Some(id) } => match self.plans.iter().position(|p| p.id == id) {
                Some(index) => done(ControlResponse::ok(self.plan_json(index))),
                None => done(ControlResponse::error(format!("no plan {id}"))),
            },
            ControlRequest::PlanUpdate { plan: _, id, status, note, session } => match self.plans[target].task_mut(&id)
            {
                None => done(ControlResponse::error(format!("no task {id}"))),
                Some(task) => {
                    if let Some(status) = &status {
                        match TaskStatus::parse(status) {
                            Some(status) => task.set_status(status),
                            None => {
                                let _ = reply.send(done(ControlResponse::error(format!("unknown status {status}"))));
                                return;
                            }
                        }
                    }
                    if let Some(note) = note {
                        task.notes.push(note);
                    }
                    if let Some(reference) = session {
                        let found = self
                            .sessions
                            .iter()
                            .find(|s| s.id == reference || s.id.starts_with(&reference) || s.title == reference);
                        if let (Some(found), Some(task)) =
                            (found.map(|s| s.id.clone()), self.plans[target].task_mut(&id))
                        {
                            task.session = Some(found);
                        }
                    }
                    self.save_plan(target);
                    done(ControlResponse::ok(self.plan_json(target)))
                }
            },
            ControlRequest::PlanDispatch { plan: _, id } => match self.dispatch(target, id.as_deref()) {
                Ok(started) => done(ControlResponse::ok(json!({ "started": started }))),
                Err(err) => done(ControlResponse::error(err)),
            },
            ControlRequest::PlanIntegrate { plan: _, id } => {
                // Committing and merging can take a while: answer once it is done.
                self.integrate(target, &id, Some(reply));
                return;
            }
            ControlRequest::Report { pane, status, summary } => {
                let task = self.report(pane.as_deref(), &status, &summary);
                done(ControlResponse::ok(json!({ "recorded": true, "task": task })))
            }
            ControlRequest::InlayHints { session } => match self.find(&session) {
                None => missing(&session),
                Some(index) => match self.editors.get(&self.sessions[index].root()) {
                    Some(editor) => {
                        editor.type_steps(self.config.editor.inlay_hints_keys.clone(), Duration::ZERO);
                        done(ControlResponse::ok(json!({ "toggled": true })))
                    }
                    None => done(ControlResponse::error("no editor open for that worktree")),
                },
            },
            ControlRequest::OpenProject { path } => {
                self.open_project(&paths::expand(&path));
                done(ControlResponse::ok(json!({ "projects": self.open_projects })))
            }
            ControlRequest::Cleanup { project } => match workspace::project_root(&paths::expand(&project)) {
                Ok(root) => {
                    self.open_cleanup(root.clone(), None);
                    done(ControlResponse::ok(json!({ "cleanup": root })))
                }
                Err(err) => done(ControlResponse::error(format!("{err:#}"))),
            },
            ControlRequest::ShowDiff { open } => {
                self.diff_open = open;
                done(ControlResponse::ok(json!({ "diff_open": open })))
            }
            ControlRequest::ShowChief { open, dock, width } => {
                if width.is_some_and(|w| !w.is_finite() || w < layout::MIN_DOCK_WIDTH) {
                    done(ControlResponse::error("chief width must be at least 520 points"))
                } else {
                    if let Some(dock) = dock {
                        self.set_chief_dock(dock);
                    }
                    if let Some(width) = width {
                        self.store.chief_width = Some(width);
                        self.save_chief_preferences();
                    }
                    if open && !self.chief_open {
                        self.toggle_chief(ctx);
                    } else if !open {
                        self.chief_open = false;
                    }
                    done(ControlResponse::ok(
                        json!({ "open": self.chief_open, "dock": self.chief_dock(), "width": self.store.chief_width }),
                    ))
                }
            }
            ControlRequest::Resize { width, height } => {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(vec2(width, height)));
                done(ControlResponse::ok(json!({ "requested": [width, height] })))
            }
            ControlRequest::Start { agent, folder, repo, branch, prompt, args } => {
                let folder = match (folder, repo, branch) {
                    (_, Some(repo), Some(branch)) => {
                        // Checking out a big repository takes a while: do it off the UI thread
                        // and start the agent (and answer) once the worktree exists.
                        self.create_worktree(
                            paths::expand(&repo),
                            branch,
                            None,
                            Launch { agent, prompt, args, reply: Some(reply), task: None },
                        );
                        return;
                    }
                    (Some(folder), _, _) => Ok(paths::expand(&folder)),
                    _ => Err("give a folder, or a repo and a branch for a new worktree".to_string()),
                };
                match folder.and_then(|folder| self.start(ctx, &agent, folder, None, args, prompt)) {
                    Ok(id) => {
                        let index = self.find(&id).expect("just started");
                        done(ControlResponse::ok(self.describe(index)))
                    }
                    Err(err) => done(ControlResponse::error(err)),
                }
            }
            ControlRequest::Send { session, text, raw } => match self.find(&session) {
                None => missing(&session),
                Some(index)
                    if raw && (self.sessions[index].approval_hold || !self.sessions[index].approvals.is_empty()) =>
                {
                    done(ControlResponse::error("raw input is blocked while permission is pending; use imacs_approve"))
                }
                Some(index) if raw => match &self.sessions[index].terminal {
                    Some(terminal) => {
                        terminal.write(unescape(&text).into_bytes());
                        done(ControlResponse::ok(json!({ "sent": true })))
                    }
                    None => done(ControlResponse::error("raw keys need a session running inside IMACS")),
                },
                Some(index) => done(match self.deliver(index, &text) {
                    Delivery::Held => {
                        ControlResponse::ok(json!({"sent":false, "held":true, "held_until":"permission answered"}))
                    }
                    Delivery::Sent | Delivery::Queued => ControlResponse::ok(
                        json!({"sent":true, "queued_until_next_hook":self.sessions[index].terminal.is_none()}),
                    ),
                    Delivery::Unavailable => ControlResponse::error("session cannot receive messages (exited?)"),
                }),
            },
            ControlRequest::Read { session, lines } => match self.find(&session) {
                None => missing(&session),
                Some(index) => match &self.sessions[index].terminal {
                    Some(terminal) => done(ControlResponse::ok(json!({
                        "status": self.sessions[index].status.label(),
                        "screen": terminal.screen_text(lines.unwrap_or(60)),
                    }))),
                    None => done(ControlResponse::ok(json!({
                        "status": self.sessions[index].status.label(),
                        "last_prompt": self.sessions[index].last_prompt,
                        "last_message": self.sessions[index].last_message,
                    }))),
                },
            },
            ControlRequest::Diff { session, patch } => match self.find(&session) {
                None => missing(&session),
                Some(index) => {
                    let root = self.sessions[index].root();
                    self.diffs.entry(root.clone()).or_default().mark_dirty();
                    ControlOutcome::Diff {
                        root,
                        base_ref: self.config.base_ref.clone(),
                        patch,
                        session: json!(self.sessions[index].id),
                    }
                }
            },
            ControlRequest::Comment { session, path, line, removed, text } => match self.find(&session) {
                None => missing(&session),
                Some(index) => {
                    let root = self.sessions[index].root();
                    let state = self.diffs.entry(root.clone()).or_default();
                    let file = state.prepared.as_ref().and_then(|p| p.diff.files.iter().find(|f| f.path() == path));
                    let excerpt = file
                        .and_then(|f| {
                            f.hunks.iter().flat_map(|h| &h.lines).find(|l| {
                                let number = if removed { l.old_no } else { l.new_no };
                                line.is_some()
                                    && number == line
                                    && (l.kind == imacs_core::diff::LineKind::Removed) == removed
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
                    state.comments.push(steer::ReviewComment { path, line, removed, excerpt, body: text });
                    done(ControlResponse::ok(json!({ "pending_comments": state.comments.len() })))
                }
            },
            ControlRequest::SendReview { session } => match self.find(&session) {
                None => missing(&session),
                Some(index) => {
                    let root = self.sessions[index].root();
                    let count = self.diffs.get(&root).map(|d| d.comments.len()).unwrap_or(0);
                    if count == 0 {
                        done(ControlResponse::error("no pending comments"))
                    } else {
                        let delivery = self.send_review(&root);
                        let left = self.diffs.get(&root).map(|d| d.comments.len()).unwrap_or(0);
                        if left == 0 {
                            done(ControlResponse::ok(if delivery == Delivery::Held {
                                json!({"held_comments":count, "held":true})
                            } else {
                                json!({ "sent_comments": count })
                            }))
                        } else {
                            done(ControlResponse::error("could not deliver the review"))
                        }
                    }
                }
            },
            ControlRequest::Activate { session } => match self.find(&session) {
                None => missing(&session),
                Some(index) => {
                    let id = self.sessions[index].id.clone();
                    self.activate(&id, true);
                    done(ControlResponse::ok(json!({ "active": id })))
                }
            },
            ControlRequest::Close { session } => match self.find(&session) {
                None => missing(&session),
                Some(index) => {
                    let id = self.sessions[index].id.clone();
                    self.close(&id);
                    done(ControlResponse::ok(json!({ "closed": id })))
                }
            },
            ControlRequest::Screenshot { path } => {
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
                return;
            }
        };
        let _ = reply.send(outcome);
    }

    /// Typing into a TUI has no receipt: a message that lands while the agent is
    /// wrapping up a turn can be dropped. A message counts as delivered once the
    /// agent starts working on it or it shows in the transcript; otherwise press
    /// Enter again (text stuck in the input box) or type it again.
    fn verify_deliveries(&mut self) {
        let mut warnings = Vec::new();
        for session in &mut self.sessions {
            if session.approval_hold || !session.approvals.is_empty() || !session.held.is_empty() {
                continue;
            }
            let (Some(pending), Some(terminal)) = (session.pending.as_mut(), session.terminal.as_ref()) else {
                continue;
            };
            if terminal.exited() {
                session.pending = None;
                continue;
            }
            let started = !pending.was_working && session.status == AgentStatus::Working;
            let snippet: String = pending.text.lines().next().unwrap_or("").chars().take(24).collect();
            let screen = terminal.screen_text(200);
            let lines: Vec<&str> = screen.lines().collect();
            let input_box = lines.len().saturating_sub(6);
            let in_transcript =
                !snippet.trim().is_empty() && lines[..input_box].iter().any(|l| l.contains(snippet.trim()));
            if started || in_transcript {
                session.pending = None;
                continue;
            }
            if pending.sent.elapsed() < DELIVERY_TIMEOUT {
                continue;
            }
            if pending.attempts >= DELIVERY_ATTEMPTS {
                warnings.push(session.title.clone());
                session.pending = None;
                continue;
            }
            let in_input_box = lines[input_box..].iter().any(|l| l.contains(snippet.trim()));
            if in_input_box {
                terminal.write(b"\r".to_vec());
            } else {
                terminal.submit(&pending.text);
            }
            pending.attempts += 1;
            pending.sent = Instant::now();
            pending.was_working = session.status == AgentStatus::Working;
        }
        for title in warnings {
            self.toast(format!("{title} has not picked up your message; check its terminal"));
        }
    }

    /// Sessions in the selected project or plan view, excluding the chief.
    fn visible_sessions(&self) -> Vec<usize> {
        (0..self.sessions.len())
            .filter(|&i| !self.sessions[i].chief)
            .filter(|&i| {
                if self.plan_view {
                    self.plans[self.selected_plan].contains_session(&self.sessions[i].id)
                } else {
                    match &self.view {
                        None => true,
                        Some(project) => self.project_of(&self.sessions[i].root()).as_ref() == Some(project),
                    }
                }
            })
            .collect()
    }

    /// One tab per open project plus "All"; a project tab shows only its agents,
    /// explorer, diff and plan.
    fn view_tabs(&mut self, ui: &mut egui::Ui) {
        let p = self.palette.clone();
        let mut pick: Option<Option<PathBuf>> = None;
        let mut close_idle: Option<PathBuf> = None;
        let agents = self.sessions.iter().filter(|s| !s.chief).count();
        if ui
            .selectable_label(
                !self.plan_view && self.view.is_none(),
                RichText::new(format!("All · {agents}")).size(12.5),
            )
            .clicked()
        {
            pick = Some(None);
        }
        for root in self.repos.clone() {
            let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let count =
                self.sessions.iter().filter(|s| !s.chief && self.project_of(&s.root()).as_ref() == Some(&root)).count();
            let label = format!("{name} · {count}");
            let selected = !self.plan_view && self.view.as_ref() == Some(&root);
            let text = RichText::new(label).size(12.5).color(if selected { p.accent } else { p.foreground });
            let response = ui.selectable_label(selected, text);
            if response.clicked() {
                pick = Some(Some(root.clone()));
            }
            response.context_menu(|ui| {
                if ui.button("Close idle sessions").on_hover_text("Idle or exited sessions no plan task uses").clicked()
                {
                    close_idle = Some(root.clone());
                    ui.close();
                }
            });
        }
        let mut plan_pick = None;
        for (index, plan) in self.plans.iter().enumerate().filter(|(_, p)| p.is_open()) {
            let name: String = plan.goal.chars().take(28).collect();
            let marker = if self.changed_plans.contains(&plan.id) { " *" } else { "" };
            let label = format!("● {name} · {}/{}{marker}", plan.count(TaskStatus::Done), plan.tasks.len());
            let attention = if self.sessions.iter().any(|s| plan.contains_session(&s.id) && s.status.needs_attention())
            {
                "needs you"
            } else {
                plan.attention()
            };
            let color = match attention {
                "needs you" => p.warning,
                "all done" => p.success,
                _ => p.accent,
            };
            if ui
                .selectable_label(
                    self.plan_view && self.selected_plan == index,
                    RichText::new(label).size(12.5).color(color),
                )
                .on_hover_text(format!("{}: {}", plan.goal, attention))
                .clicked()
            {
                plan_pick = Some(index);
            }
        }
        if let Some(index) = plan_pick {
            self.selected_plan = index;
            self.show_plan_fleet();
        }
        if let Some(view) = pick {
            self.set_view(view);
        }
        if let Some(root) = close_idle {
            let closed = self.close_idle(&root);
            self.toast(format!("Closed {closed} idle session{}", if closed == 1 { "" } else { "s" }));
        }
    }

    fn set_view(&mut self, view: Option<PathBuf>) {
        self.plan_view = false;
        self.view = view.clone();
        let Some(project) = view else { return };
        let in_view = self.active_index().is_some_and(|i| {
            !self.sessions[i].chief && self.project_of(&self.sessions[i].root()).as_ref() == Some(&project)
        });
        if !in_view {
            match self.visible_sessions().first().map(|&i| self.sessions[i].id.clone()) {
                Some(id) => self.activate(&id, false),
                None => {
                    self.active = None;
                    self.diff_root = Some(project);
                }
            }
        }
    }

    fn show_plan_fleet(&mut self) {
        self.changed_plans.remove(&self.plans[self.selected_plan].id);
        self.ctx.request_repaint();
        self.view = self.plans[self.selected_plan].project.clone();
        self.plan_view = true;
        self.panel = PanelMode::Diff;
        if self.chief_dock() == ChiefDock::Floating || self.last_layout.is_some_and(|layout| layout.dock_width == 0.0) {
            self.chief_open = false;
        }
        let visible = self.visible_sessions();
        if !self.active_index().is_some_and(|i| visible.contains(&i)) {
            if let Some(&i) = visible.first() {
                self.activate(&self.sessions[i].id.clone(), false);
            } else {
                self.active = None;
                self.diff_root = None;
            }
        }
    }

    /// Closes the project's idle and exited agents that no plan task is using. Shells
    /// are always idle to IMACS, so they are left alone.
    fn close_idle(&mut self, project: &Path) -> usize {
        let in_plan: HashSet<String> =
            self.plans.iter().flat_map(|p| &p.tasks).filter_map(|t| t.session.clone()).collect();
        let idle: Vec<String> = self
            .sessions
            .iter()
            .filter(|s| !s.chief && s.is_agent() && matches!(s.status, AgentStatus::Idle | AgentStatus::Exited))
            .filter(|s| !in_plan.contains(&s.id))
            .filter(|s| self.project_of(&s.root()).as_deref() == Some(project))
            .map(|s| s.id.clone())
            .collect();
        for id in &idle {
            self.close(id);
        }
        idle.len()
    }

    fn chief_dock(&self) -> ChiefDock {
        self.store.chief_dock.unwrap_or(self.config.chief.dock)
    }

    fn save_chief_preferences(&mut self) {
        if let Err(err) = self.store.save(&paths::store_file()) {
            self.toast(format!("Could not save chief preferences: {err:#}"));
        }
    }

    fn set_chief_dock(&mut self, dock: ChiefDock) {
        self.store.chief_dock = Some(dock);
        self.save_chief_preferences();
        if self.chief_open {
            self.focus_terminal = self.sessions.iter().find(|s| s.chief).map(|s| s.id.clone());
        }
        self.ctx.request_repaint();
    }

    fn toggle_chief(&mut self, ctx: &egui::Context) {
        if !self.sessions.iter().any(|s| s.chief && s.terminal.as_ref().is_some_and(|t| !t.exited())) {
            self.sessions.retain(|s| !s.chief);
            if let Err(err) = self.start_chief(ctx, None) {
                self.toast(err);
                return;
            }
            self.chief_open = true;
        } else {
            self.chief_open = !self.chief_open;
        }
        if self.chief_open
            && let Some(chief) = self.sessions.iter().find(|s| s.chief)
        {
            self.focus_terminal = Some(chief.id.clone());
        }
    }

    /// Push-to-talk: the first press records, the second transcribes and sends.
    fn toggle_voice(&mut self) {
        match self.voice {
            Voice::Idle => {
                let cancelled = Arc::new(AtomicBool::new(false));
                self.voice = Voice::Opening(cancelled.clone());
                let (config, tx, ctx) = (self.config.voice.clone(), self.voice_tx.clone(), self.ctx.clone());
                std::thread::spawn(move || {
                    let recording = Recorder::start().and_then(|recorder| {
                        config.api_key(env_var).map_err(|err| format!("{err:#}"))?;
                        Ok(recorder)
                    });
                    if !cancelled.load(Ordering::Relaxed) {
                        let _ = tx.send(VoiceEvent::Opened(cancelled, recording));
                        ctx.request_repaint();
                    }
                });
            }
            Voice::Opening(ref cancelled) => {
                cancelled.store(true, Ordering::Relaxed);
                self.voice = Voice::Idle;
                self.toast("Voice: recording cancelled");
            }
            Voice::Recording(_) => self.stop_voice(),
            Voice::Transcribing => self.toast("Voice: still transcribing the last recording"),
        }
    }

    /// Stops recording and transcribes on a background thread.
    fn stop_voice(&mut self) {
        let Voice::Recording(recorder) = std::mem::replace(&mut self.voice, Voice::Transcribing) else { return };
        let recording = recorder.finish();
        let (config, tx, ctx) = (self.config.voice.clone(), self.voice_tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let transcript = recording.and_then(|(samples, rate)| {
                if samples.len() < rate as usize / 5 {
                    return Err("nothing recorded".to_string());
                }
                if imacs_core::voice::is_silent(&samples) {
                    return Err("the microphone recorded only silence (muted, or no microphone access in System \
                                Settings › Privacy & Security?)"
                        .to_string());
                }
                let (samples, rate) = imacs_core::voice::downsample(&samples, rate, imacs_core::voice::SPEECH_RATE);
                let wav = imacs_core::voice::wav(&samples, rate);
                let key = config.api_key(env_var).map_err(|e| format!("{e:#}"))?;
                imacs_core::voice::transcribe(&config, &key, &wav).map_err(|e| format!("{e:#}"))
            });
            let _ = tx.send(VoiceEvent::Transcript(transcript));
            ctx.request_repaint();
        });
    }

    /// Sends a transcript the way the steer bar does: to the chief (started and shown
    /// if needed) or to the active agent. Text that cannot be sent lands in the steer bar.
    fn send_voice(&mut self, ctx: &egui::Context, text: &str) {
        if text.is_empty() {
            self.toast("Voice: heard nothing");
            return;
        }
        let quoted = format!("“{}”", truncate(text, 60));
        let failed = match self.config.voice.target {
            VoiceTarget::Chief => {
                let started = match self.sessions.iter().position(|s| s.chief && s.can_receive()) {
                    Some(index) => {
                        self.deliver(index, text);
                        Ok(())
                    }
                    None => {
                        self.sessions.retain(|s| !s.chief);
                        self.start_chief(ctx, Some(text.to_string())).map(|_| ())
                    }
                };
                match started {
                    Ok(()) => {
                        self.chief_open = true;
                        self.focus_terminal = self.sessions.iter().find(|s| s.chief).map(|s| s.id.clone());
                        self.toast(format!("Sent to the chief: {quoted}"));
                        None
                    }
                    Err(err) => Some(err),
                }
            }
            VoiceTarget::Active => {
                match self.active_index().filter(|&i| self.sessions[i].is_agent() && self.sessions[i].can_receive()) {
                    Some(index) if self.deliver(index, text) != Delivery::Unavailable => {
                        self.toast(format!("Sent to {}: {quoted}", self.sessions[index].title));
                        None
                    }
                    _ => Some("No active agent to send to".to_string()),
                }
            }
        };
        if let Some(err) = failed {
            if !self.steer_text.trim().is_empty() {
                self.steer_text.push('\n');
            }
            self.steer_text.push_str(text);
            self.focus_steer = true;
            self.toast(format!("{err}; the transcript is in the steer bar"));
        }
    }

    /// The chief pill and floating window; the dock is part of the main layout.
    fn chief_ui(&mut self, ctx: &egui::Context) {
        let p = self.palette.clone();
        let chief = self.sessions.iter().position(|s| s.chief);
        let status = chief.map(|i| self.sessions[i].status);
        let mut toggle = false;
        let mut dock = self.chief_dock();
        egui::Area::new(egui::Id::NULL.with("chief-pill"))
            .anchor(egui::Align2::CENTER_TOP, vec2(0.0, 4.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                let stroke = if self.chief_open { p.accent } else { lerp(p.border, p.accent, 0.5) };
                let response = egui::Frame::new()
                    .fill(lerp(p.raised, p.accent, 0.08))
                    .stroke(Stroke::new(1.0, stroke))
                    .corner_radius(13)
                    .inner_margin(egui::Margin::symmetric(14, 3))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let (icon, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                            orbit_icon(ui.painter(), icon, p.accent, p.foreground);
                            ui.label(RichText::new("CHIEF").strong().color(p.accent).size(13.0));
                            match status {
                                Some(status) => {
                                    let (dot, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                                    let time = ui.input(|i| i.time);
                                    ui.painter().circle_filled(dot.center(), 3.5, status_dot(status, &p, time));
                                    ui.label(RichText::new(status.label()).color(p.muted).size(11.5));
                                }
                                None => {
                                    ui.label(RichText::new("start").color(p.muted).size(11.5));
                                }
                            }
                            if !self.plans[self.selected_plan].tasks.is_empty() {
                                let done = self.plans[self.selected_plan].count(TaskStatus::Done);
                                ui.label(
                                    RichText::new(format!(
                                        "plan {done}/{}",
                                        self.plans[self.selected_plan].tasks.len()
                                    ))
                                    .color(p.muted)
                                    .size(11.5),
                                );
                            }
                            match &self.voice {
                                Voice::Recording(recorder) => {
                                    let secs = recorder.elapsed().as_secs();
                                    let text = format!("● REC {}:{:02}", secs / 60, secs % 60);
                                    ui.label(RichText::new(text).color(p.error).strong().size(11.5));
                                }
                                Voice::Opening(_) | Voice::Transcribing => {
                                    ui.add(egui::Spinner::new().size(12.0).color(p.accent));
                                }
                                Voice::Idle => {}
                            }
                        });
                    })
                    .response
                    .interact(Sense::click());
                let keys = &self.config.keys;
                let hover = match self.voice {
                    Voice::Opening(_) => "Opening microphone: Esc cancels".to_string(),
                    Voice::Recording(_) => format!("Recording: {} sends, Esc cancels", keys.voice),
                    Voice::Transcribing => "Transcribing".to_string(),
                    Voice::Idle => format!("The chief agent ({}) · voice ({})", keys.toggle_chief, keys.voice),
                };
                response.context_menu(|ui| {
                    if ui.selectable_value(&mut dock, ChiefDock::Floating, "Float chief").clicked()
                        || ui.selectable_value(&mut dock, ChiefDock::Docked, "Dock chief").clicked()
                    {
                        ui.close();
                    }
                });
                if response.on_hover_text(hover).clicked() {
                    toggle = true;
                }
            });
        if dock != self.chief_dock() {
            self.set_chief_dock(dock);
        }
        if toggle {
            self.toggle_chief(ctx);
        }

        if !self.chief_open || self.last_layout.is_some_and(|layout| layout.dock_width > 0.0) {
            return;
        }
        let screen = ctx.content_rect();
        egui::Window::new("CHIEF AGENT")
            .id(egui::Id::NULL.with("chief-window"))
            .title_bar(false)
            .resizable(true)
            .default_size(vec2(screen.width() * 0.5, screen.height() * 0.66))
            .default_pos(screen.center())
            .pivot(egui::Align2::CENTER_CENTER)
            .frame(
                egui::Frame::window(&ctx.global_style())
                    .fill(p.background)
                    .stroke(Stroke::new(1.5, p.accent.gamma_multiply(0.8))),
            )
            .show(ctx, |ui| self.chief_content(ui));
    }

    fn chief_content(&mut self, ui: &mut egui::Ui) {
        let Some(index) = self.sessions.iter().position(|s| s.chief) else { return };
        let p = self.palette.clone();
        ui.horizontal(|ui| {
            ui.label(RichText::new("◆ CHIEF AGENT").color(p.accent).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("minimize").on_hover_text(&self.config.keys.toggle_chief).clicked() {
                    self.chief_open = false;
                }
                let dock = self.chief_dock();
                let label = if dock == ChiefDock::Floating { "dock" } else { "float" };
                if ui.small_button(label).clicked() {
                    self.set_chief_dock(if dock == ChiefDock::Floating {
                        ChiefDock::Docked
                    } else {
                        ChiefDock::Floating
                    });
                }
            });
        });
        ui.separator();
        let metrics = self.metrics(ui);
        let rect = ui.available_rect_before_wrap();
        ui.allocate_rect(rect, Sense::hover());
        let session = &mut self.sessions[index];
        let focus = self.focus_terminal.as_deref() == Some(session.id.as_str());
        if let Some(terminal) = &mut session.terminal {
            let response = terminal.show(ui, rect, egui::Id::NULL.with(("chief-term", &session.id)), &metrics, &p);
            if focus {
                response.request_focus();
                self.focus_terminal = None;
            }
        }
    }

    fn toggle_plan(&mut self) {
        self.ctx.request_repaint();
        if self.panel == PanelMode::Plan && self.diff_open {
            self.panel = PanelMode::Diff;
        } else {
            self.panel = PanelMode::Plan;
            self.diff_open = true;
        }
    }

    fn plan_panel(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let p = self.palette.clone();
        ui.painter().rect_filled(rect, 0.0, p.background);
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .id_salt(("plan-panel", &self.plans[self.selected_plan].id))
                .max_rect(rect.shrink2(vec2(12.0, 8.0))),
        );
        let ui = &mut child;
        let mut approve = false;
        let mut dispatch_all = false;
        let mut close_status = None;
        let mut close = false;
        let ready = self.plans[self.selected_plan].ready().len();
        ui.horizontal(|ui| {
            ui.label(RichText::new("PLAN").color(p.accent).strong().size(13.0));
            let total = self.plans[self.selected_plan].tasks.len();
            if total > 0 {
                let done = self.plans[self.selected_plan].count(TaskStatus::Done);
                ui.label(RichText::new(format!("{done}/{total} done")).color(p.muted).size(12.0));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.plans[self.selected_plan].is_open() {
                    if ui.small_button("Archive plan").clicked() {
                        close_status = Some(PlanStatus::Archived);
                    }
                    if ui.small_button("Complete plan").clicked() {
                        close_status = Some(PlanStatus::Completed);
                    }
                }
                if ui.small_button("×").on_hover_text("Back to the diff").clicked() {
                    close = true;
                }
                if total == 0 {
                    return;
                }
                if !self.plans[self.selected_plan].approved {
                    let button = egui::Button::new(RichText::new("Approve plan").color(Color32::WHITE))
                        .fill(p.accent.gamma_multiply(0.7));
                    if ui.add(button).on_hover_text("Let the orchestrator dispatch these tasks").clicked() {
                        approve = true;
                    }
                } else {
                    let button = egui::Button::new(format!("Dispatch ready ({ready})"));
                    if ui
                        .add_enabled(ready > 0, button)
                        .on_hover_text("Start every task whose dependencies are done")
                        .clicked()
                    {
                        dispatch_all = true;
                    }
                    ui.label(RichText::new("approved ✓").color(p.success).size(12.0));
                }
            });
        });
        if !self.plans[self.selected_plan].goal.is_empty() {
            ui.label(RichText::new(&self.plans[self.selected_plan].goal).color(p.foreground).size(13.0));
        }
        let integration = self.plans[self.selected_plan].integration_branch();
        if !self.plans[self.selected_plan].tasks.is_empty() {
            ui.label(RichText::new(format!("integration branch: {integration}")).color(p.muted).size(11.5))
                .on_hover_text("Integrated tasks are merged into it, and tasks with dependencies branch from it");
        }
        for (task, dep) in self.plans[self.selected_plan].unknown_dependencies() {
            ui.label(RichText::new(format!("{task} depends on unknown task {dep}")).color(p.error).size(11.5));
        }
        ui.add_space(6.0);
        if self.plans[self.selected_plan].tasks.is_empty() {
            ui.label(
                RichText::new(
                    "No plan yet. Ask an orchestrator (a Claude session with the IMACS MCP server) to plan a goal: \
                     its tasks appear here for you to review, edit and approve before any agent starts.",
                )
                .color(p.muted),
            );
        }

        let mut actions: Vec<(String, &str)> = Vec::new();
        let mut open_session: Option<String> = None;
        let editable = !self.plans[self.selected_plan].approved;
        let ready_ids: HashSet<String> = self.plans[self.selected_plan].ready().iter().map(|t| t.id.clone()).collect();
        let approved = self.plans[self.selected_plan].approved;
        let sessions: HashMap<String, (String, AgentStatus)> =
            self.sessions.iter().map(|s| (s.id.clone(), (s.title.clone(), s.status))).collect();
        let expanded = &mut self.expanded_tasks;
        let integrating = &self.integrating;
        let plan_id = self.plans[self.selected_plan].id.clone();
        let tasks = &mut self.plans[self.selected_plan].tasks;
        if !tasks.is_empty() {
            ui.columns(TaskStatus::ALL.len(), |columns| {
                for (column, status) in columns.iter_mut().zip(TaskStatus::ALL) {
                    let count = tasks.iter().filter(|t| t.status == status).count();
                    let color = task_color(status, &p);
                    column.label(
                        RichText::new(format!("{} · {count}", status.label().to_uppercase()))
                            .color(color)
                            .strong()
                            .size(11.0),
                    );
                    column.add_space(4.0);
                    egui::ScrollArea::vertical()
                        .id_salt(("plan-column", &plan_id, status.label()))
                        .auto_shrink([false, false])
                        .show(column, |ui| {
                            for task in tasks.iter_mut().filter(|t| t.status == status) {
                                let open = expanded.contains(&(plan_id.clone(), task.id.clone()));
                                egui::Frame::new()
                                    .fill(p.raised)
                                    .stroke(Stroke::new(1.0, if open { color.gamma_multiply(0.6) } else { p.border }))
                                    .corner_radius(3)
                                    .inner_margin(egui::Margin::same(8))
                                    .show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        let title = ui.add(
                                            egui::Label::new(RichText::new(&task.title).color(p.foreground).strong())
                                                .sense(Sense::click()),
                                        );
                                        if title.clicked() {
                                            actions.push((task.id.clone(), "toggle"));
                                        }
                                        let mut meta = format!(
                                            "{} · {}",
                                            task.id,
                                            if task.agent.is_empty() { "claude" } else { &task.agent }
                                        );
                                        if let Some(model) = task
                                            .args
                                            .windows(2)
                                            .find(|w| w[0] == "-m" || w[0] == "--model")
                                            .map(|w| w[1].clone())
                                        {
                                            meta.push_str(&format!(" {model}"));
                                        }
                                        if let Some(branch) = &task.branch {
                                            meta.push_str(&format!(" · {branch}"));
                                        }
                                        ui.label(RichText::new(meta).color(p.muted).size(11.0));
                                        if !task.depends_on.is_empty() {
                                            ui.label(
                                                RichText::new(format!("after {}", task.depends_on.join(", ")))
                                                    .color(p.muted)
                                                    .size(11.0),
                                            );
                                        }
                                        if let Some((title, agent_status)) =
                                            task.session.as_ref().and_then(|s| sessions.get(s))
                                        {
                                            ui.label(
                                                RichText::new(format!("● {title} · {}", agent_status.label()))
                                                    .color(status_color(*agent_status, &p))
                                                    .size(11.0),
                                            );
                                        }
                                        if let Some(report) = &task.report {
                                            let color = match report.status.as_str() {
                                                "done" => p.success,
                                                "blocked" | "question" => p.warning,
                                                _ => p.info,
                                            };
                                            let summary = if open {
                                                report.summary.clone()
                                            } else {
                                                truncate(&report.summary, 160)
                                            };
                                            ui.label(
                                                RichText::new(format!("{}: {summary}", report.status))
                                                    .color(color)
                                                    .size(12.0),
                                            );
                                        }
                                        if let Some(note) = task.notes.last() {
                                            ui.label(RichText::new(note).color(p.muted).italics().size(11.0));
                                        }
                                        if open {
                                            ui.add_space(4.0);
                                            if editable && task.status == TaskStatus::Planned {
                                                let edit = egui::TextEdit::multiline(&mut task.brief)
                                                    .desired_rows(4)
                                                    .desired_width(f32::INFINITY);
                                                if ui.add(edit).changed() {
                                                    actions.push((task.id.clone(), "edited"));
                                                }
                                            } else {
                                                ui.label(RichText::new(&task.brief).color(p.foreground).size(12.0));
                                            }
                                            for check in &task.acceptance {
                                                ui.label(RichText::new(format!("✓ {check}")).color(p.muted).size(11.5));
                                            }
                                        }
                                        ui.horizontal_wrapped(|ui| {
                                            if task.session.is_some() && ui.small_button("open").clicked() {
                                                open_session = task.session.clone();
                                            }
                                            match task.status {
                                                TaskStatus::Planned if approved && ready_ids.contains(&task.id) => {
                                                    if ui.small_button("start").clicked() {
                                                        actions.push((task.id.clone(), "start"));
                                                    }
                                                }
                                                TaskStatus::Review => {
                                                    if ui.small_button("accept").clicked() {
                                                        actions.push((task.id.clone(), "done"));
                                                    }
                                                    if ui
                                                        .small_button("send back")
                                                        .on_hover_text("Back to running; steer it in the steer bar")
                                                        .clicked()
                                                    {
                                                        actions.push((task.id.clone(), "running"));
                                                    }
                                                }
                                                TaskStatus::Blocked => {
                                                    if ui.small_button("unblock").clicked() {
                                                        actions.push((task.id.clone(), "running"));
                                                    }
                                                }
                                                TaskStatus::Done if ui.small_button("reopen").clicked() => {
                                                    actions.push((task.id.clone(), "review"));
                                                }
                                                _ => {}
                                            }
                                            if matches!(task.status, TaskStatus::Review | TaskStatus::Done) {
                                                let busy = integrating.contains(&(plan_id.clone(), task.id.clone()));
                                                let label = if busy { "integrating…" } else { "integrate" };
                                                if ui
                                                    .add_enabled(!busy, egui::Button::new(label).small())
                                                    .on_hover_text(format!(
                                                        "Commit its changes and merge it into {integration}"
                                                    ))
                                                    .clicked()
                                                {
                                                    actions.push((task.id.clone(), "integrate"));
                                                }
                                            }
                                        });
                                    });
                                ui.add_space(6.0);
                            }
                        });
                }
            });
        }

        let mut changed = false;
        for (id, action) in actions {
            match action {
                "toggle" => {
                    let key = (plan_id.clone(), id);
                    if !self.expanded_tasks.remove(&key) {
                        self.expanded_tasks.insert(key);
                    }
                }
                "edited" => changed = true,
                "start" => {
                    if let Err(err) = self.dispatch(self.selected_plan, Some(&id)) {
                        self.toast(err);
                    }
                }
                "integrate" => self.integrate(self.selected_plan, &id, None),
                status => {
                    if let (Some(task), Some(status)) =
                        (self.plans[self.selected_plan].task_mut(&id), TaskStatus::parse(status))
                    {
                        task.set_status(status);
                        changed = true;
                    }
                }
            }
        }
        if approve {
            self.plans[self.selected_plan].approved = true;
            changed = true;
            self.toast("Plan approved: the orchestrator can dispatch it now");
        }
        if dispatch_all && let Err(err) = self.dispatch(self.selected_plan, None) {
            self.toast(err);
        }
        if changed {
            self.save_plan(self.selected_plan);
        }
        if let Some(session) = open_session {
            self.activate(&session, true);
            self.panel = PanelMode::Diff;
        }
        if close {
            self.panel = PanelMode::Diff;
        }
        if let Some(status) = close_status
            && let Err(err) = self.close_plan(self.selected_plan, status)
        {
            self.toast(err);
        }
    }

    fn close_plan(&mut self, index: usize, status: PlanStatus) -> Result<(), String> {
        if status == PlanStatus::Open {
            return Err("Choose completed or archived.".into());
        }
        let mut closed = self.plans[index].clone();
        closed.status = status;
        closed.save_in(&paths::imacs_home()).map_err(|err| format!("Closing plan failed: {err:#}"))?;
        self.plans[index] = closed;
        self.changed_plans.remove(&self.plans[index].id);
        if self.selected_plan == index {
            self.selected_plan = self.plans.iter().position(Plan::is_open).unwrap_or_else(|| {
                self.plans.push(Plan::default());
                self.plans.len() - 1
            });
            if self.plan_view {
                if self.plans[self.selected_plan].is_open() {
                    self.show_plan_fleet();
                } else {
                    self.set_view(None);
                }
                self.panel = PanelMode::Diff;
            }
        }
        self.ctx.request_repaint();
        Ok(())
    }

    fn save_plan(&self, index: usize) {
        if self.plans[index].id.is_empty() {
            return;
        }
        if let Err(err) = self.plans[index].save_in(&paths::imacs_home()) {
            eprintln!("IMACS: saving the plan failed: {err:#}");
        }
    }

    fn plan_json(&self, index: usize) -> serde_json::Value {
        let mut value = serde_json::to_value(&self.plans[index]).unwrap_or_default();
        value["ready"] = json!(self.plans[index].ready().iter().map(|t| &t.id).collect::<Vec<_>>());
        value["integration"] = json!(self.plans[index].integration_branch());
        value
    }

    /// The repository a task works in.
    fn task_repo(&self, index: usize, task: &Task) -> Option<PathBuf> {
        task.repo
            .as_deref()
            .map(paths::expand)
            .or_else(|| {
                let session = task.session.as_ref().and_then(|id| self.sessions.iter().find(|s| &s.id == id))?;
                self.project_of(&session.root())
            })
            .or_else(|| self.plans[index].project.clone())
            .or_else(|| self.diff_target().and_then(|t| self.project_of(&t)))
            .or_else(|| self.repos.first().cloned())
    }

    /// Starts `only`, or every ready task, each in a new worktree with its brief as
    /// the agent's first message.
    fn dispatch(&mut self, index: usize, only: Option<&str>) -> Result<Vec<String>, String> {
        if !self.plans[index].approved {
            return Err("The plan is not approved yet: the user approves it on the plan board.".into());
        }
        let into = self.plans[index].integration_branch();
        let stale: Vec<String> = self.plans[index]
            .tasks
            .iter()
            .filter(|t| t.integrated.as_deref() == Some(&into))
            .filter(|task| {
                !matches!(task.status, TaskStatus::Review | TaskStatus::Done)
                    || !task.branch.as_deref().zip(self.task_repo(index, task)).is_some_and(|(branch, repo)| {
                        git::git(
                            &repo,
                            &[
                                "merge-base",
                                "--is-ancestor",
                                &format!("refs/heads/{branch}"),
                                &format!("refs/heads/{into}"),
                            ],
                        )
                        .is_ok()
                    })
            })
            .map(|t| t.id.clone())
            .collect();
        for id in &stale {
            self.plans[index].task_mut(id).expect("checked task").integrated = None;
        }
        if !stale.is_empty() {
            self.save_plan(index);
        }
        let ready: Vec<Task> =
            self.plans[index].ready().into_iter().filter(|t| only.is_none_or(|id| t.id == id)).cloned().collect();
        if ready.is_empty() {
            if let Some(task) = self.plans[index].tasks.iter().find(|task| {
                only.is_none_or(|id| task.id == id)
                    && task.status == TaskStatus::Planned
                    && !self.plans[index].unintegrated_dependencies(task).is_empty()
            }) {
                return Err(format!(
                    "Task {} needs integrated dependencies: {}. Use imacs_plan_integrate on them first.",
                    task.id,
                    self.plans[index].unintegrated_dependencies(task).join(", ")
                ));
            }
            return Err(match only {
                Some(id) => format!("Task {id} is not ready (it must be planned with all dependencies done)."),
                None => "No task is ready.".into(),
            });
        }
        let mut started = Vec::new();
        for task in ready {
            let Some(repo) = self.task_repo(index, &task) else {
                return Err("No project is open to branch from; give the task a repo.".into());
            };
            let branch = task
                .branch
                .clone()
                .unwrap_or_else(|| format!("imacs/{}-{}-{}", self.plans[index].id, task.id, short_slug(&task.title)));
            let agent = if task.agent.trim().is_empty() { "claude".to_string() } else { task.agent.clone() };
            let mut brief = task.compose_brief(&self.plans[index].goal);
            if task.id == imacs_core::plan::REVIEW_TASK {
                // The reviewer reads every task's branch from the shared repository.
                let base = &self.config.base_ref;
                brief.push_str("\n\nBranches to review (all in this repository):\n");
                let integration = self.plans[index].integration_branch();
                brief.push_str(&format!(
                    "- integration branch {integration} (git diff {base}...{integration}): all accepted work, review this first\n"
                ));
                for other in self.plans[index].tasks.iter().filter(|t| task.depends_on.contains(&t.id)) {
                    let branch = other.branch.clone().unwrap_or_else(|| "(not dispatched)".into());
                    brief.push_str(&format!("- {}: {} (git diff {base}...{branch})\n", other.id, other.title));
                }
            }
            let launch = Launch {
                agent,
                prompt: Some(brief),
                args: task.args.clone(),
                reply: None,
                task: Some((self.plans[index].id.clone(), task.id.clone())),
            };
            let start = self.plans[index].start_branch(&task);
            self.create_worktree(repo.clone(), branch.clone(), start, launch);
            if let Some(t) = self.plans[index].task_mut(&task.id) {
                t.set_status(TaskStatus::Running);
                t.branch = Some(branch);
                t.repo.get_or_insert_with(|| repo.to_string_lossy().into_owned());
            }
            started.push(task.id);
        }
        if self.plan_view && self.selected_plan == index {
            self.show_plan_fleet();
        } else {
            self.changed_plans.insert(self.plans[index].id.clone());
        }
        self.save_plan(index);
        Ok(started)
    }

    /// Commits a review or done task's work and merges its branch into the plan's
    /// integration branch, off the UI thread; the outcome lands on the task.
    fn integrate(&mut self, index: usize, id: &str, reply: Option<Sender<ControlOutcome>>) {
        let job = match self.plans[index].tasks.iter().find(|t| t.id == id) {
            None => Err(format!("no task {id}")),
            Some(task) if !matches!(task.status, TaskStatus::Review | TaskStatus::Done) => {
                Err(format!("Task {id} is {}: only tasks in review or done can be integrated.", task.status.label()))
            }
            Some(_) if self.integrating.contains(&(self.plans[index].id.clone(), id.to_string())) => {
                Err(format!("Task {id} is already being integrated."))
            }
            Some(task) => match (task.branch.clone(), self.task_repo(index, task)) {
                (None, _) => Err(format!("Task {id} has no branch yet: dispatch it first.")),
                (_, None) => Err("No project is open to integrate into; give the task a repo.".into()),
                (Some(branch), Some(repo)) => Ok((repo, branch, format!("{} ({})", task.title.trim(), task.id))),
            },
        };
        let (repo, branch, message) = match job {
            Ok(job) => job,
            Err(err) => {
                match reply {
                    Some(reply) => {
                        let _ = reply.send(ControlOutcome::Done(ControlResponse::error(err)));
                    }
                    None => self.toast(err),
                }
                return;
            }
        };
        let into = self.plans[index].integration_branch();
        let into_path = git::worktree_path(&self.config.worktree_path, &repo, &into);
        let (tx, base_ref, ctx) = (self.integrate_tx.clone(), self.config.base_ref.clone(), self.ctx.clone());
        let task = id.to_string();
        let plan = self.plans[index].id.clone();
        self.integrating.insert((plan.clone(), task.clone()));
        std::thread::spawn(move || {
            let result =
                git::integrate(&repo, &branch, &message, &into, &into_path, &base_ref).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Integrated { plan, task, branch, into, result, reply });
            ctx.request_repaint();
        });
    }

    fn finish_integration(&mut self, done: Integrated) {
        let Integrated { plan, task: id, branch, into, result, reply } = done;
        let Some(index) = self.plans.iter().position(|p| p.id == plan) else { return };
        self.integrating.remove(&(plan.clone(), id.clone()));
        self.worktrees_at = None;
        let (note, response) = match result {
            Ok(Integration { merge: Merge::Conflict(files), .. }) => {
                if let Some(task) = self.plans[index].task_mut(&id) {
                    task.set_status(TaskStatus::Review);
                }
                let files = files.join(", ");
                let error = format!(
                    "Merging {branch} into {into} conflicts in {files}. The merge was aborted; task {id} stays in review."
                );
                (format!("conflicts with {into} in {files}; merge aborted"), ControlResponse::error(error))
            }
            Ok(Integration { committed, merge, worktree }) => {
                if let Some(task) = self.plans[index].task_mut(&id)
                    && matches!(task.status, TaskStatus::Review | TaskStatus::Done)
                {
                    task.integrated = Some(into.clone());
                }
                let merged = merge == Merge::Merged;
                let note = match (committed, merged) {
                    (true, _) => format!("committed and merged into {into}"),
                    (false, true) => format!("merged into {into}"),
                    (false, false) => format!("already in {into}"),
                };
                let result = json!({
                    "plan": plan,
                    "task": id,
                    "integration": into,
                    "worktree": worktree,
                    "committed": committed,
                    "merged": merged,
                });
                (note, ControlResponse::ok(result))
            }
            Err(err) => (format!("could not integrate: {err}"), ControlResponse::error(err)),
        };
        if let Some(task) = self.plans[index].task_mut(&id) {
            task.notes.push(note.clone());
            self.save_plan(index);
        }
        self.toast(format!("Task {id}: {note}"));
        if let Some(reply) = reply {
            let _ = reply.send(ControlOutcome::Done(response));
        }
    }

    /// A sub-agent's `imacs_report`: attach it to its task, move the task along, and
    /// wake anyone waiting.
    fn report(&mut self, pane: Option<&str>, status: &str, summary: &str) -> Option<String> {
        let session = pane.and_then(|p| self.sessions.iter().find(|s| s.id == p));
        let (session_id, title) =
            session.map(|s| (s.id.clone(), s.title.clone())).unwrap_or_else(|| ("".into(), "unknown agent".into()));
        let owner = self.plans.iter().enumerate().find_map(|(index, plan)| {
            plan.tasks.iter().find(|t| t.session.as_deref() == Some(&session_id)).map(|task| (index, task.id.clone()))
        });
        let task_id = owner.as_ref().map(|(_, task)| task.clone());
        let plan_id = owner.as_ref().map(|(index, _)| self.plans[*index].id.clone());
        if let Some((index, id)) = &owner {
            let task = self.plans[*index].task_mut(id).expect("task owner");
            task.report =
                Some(Report { status: status.to_string(), summary: summary.to_string(), at: imacs_core::store::now() });
            match status {
                "done" => task.set_status(TaskStatus::Review),
                "blocked" | "question" => task.set_status(TaskStatus::Blocked),
                _ => {}
            }
            self.save_plan(*index);
        }
        self.event_seq += 1;
        self.events.push(StatusEvent {
            seq: self.event_seq,
            session: session_id,
            plan: plan_id,
            task: task_id.clone(),
            title: title.clone(),
            status: session.map(|s| s.status).unwrap_or(AgentStatus::Idle),
            at: imacs_core::store::now(),
            report: Some((status.to_string(), summary.to_string(), task_id.clone())),
            approval: None,
        });
        self.serve_waiters();
        let what = task_id.as_deref().map(|t| format!("task {t}")).unwrap_or(title);
        self.toast(format!("{what} reported {status}"));
        task_id
    }

    /// Running tasks whose agent exited without reporting are blocked.
    fn sync_plan(&mut self) {
        for index in 0..self.plans.len() {
            if !self.plans[index].is_open() {
                continue;
            }
            let mut changed = false;
            for task in &mut self.plans[index].tasks {
                if task.status != TaskStatus::Running {
                    continue;
                }
                let gone = match &task.session {
                    Some(id) => {
                        self.sessions.iter().find(|s| &s.id == id).is_none_or(|s| s.status == AgentStatus::Exited)
                    }
                    None => false,
                };
                if gone {
                    task.set_status(TaskStatus::Blocked);
                    task.notes.push("the agent exited before reporting".into());
                    changed = true;
                }
            }
            if changed {
                self.save_plan(index);
            }
        }
    }

    /// Records status changes as events and answers waiters they satisfy.
    fn record_events(&mut self) {
        for session in &self.sessions {
            if self.last_status.get(&session.id) != Some(&session.status) {
                self.last_status.insert(session.id.clone(), session.status);
                self.event_seq += 1;
                self.events.push(StatusEvent {
                    seq: self.event_seq,
                    session: session.id.clone(),
                    plan: self.plans.iter().find(|p| p.contains_session(&session.id)).map(|p| p.id.clone()),
                    task: self
                        .plans
                        .iter()
                        .flat_map(|p| &p.tasks)
                        .find(|t| t.session.as_deref() == Some(&session.id))
                        .map(|t| t.id.clone()),
                    title: session.title.clone(),
                    status: session.status,
                    at: imacs_core::store::now(),
                    report: None,
                    approval: session.approvals.first().map(|pending| pending.request.clone()),
                });
            }
        }
        if self.events.len() > 1000 {
            self.events.drain(..self.events.len() - 1000);
        }
        self.serve_waiters();
    }

    fn serve_waiters(&mut self) {
        let now = Instant::now();
        let mut pending = Vec::new();
        for waiter in std::mem::take(&mut self.waiters) {
            let matching: Vec<serde_json::Value> = self
                .events
                .iter()
                .filter(|e| e.seq > waiter.after)
                .filter(|e| waiter.sessions.is_empty() || waiter.sessions.contains(&e.session))
                .filter(|e| waiter.statuses.iter().any(|s| s == e.kind()))
                .map(StatusEvent::to_json)
                .collect();
            let timed_out = now >= waiter.deadline;
            if matching.is_empty() && !timed_out {
                pending.push(waiter);
                continue;
            }
            let sessions: Vec<_> = self
                .sessions
                .iter()
                .map(|s| json!({ "id": s.id, "title": s.title, "status": s.status.label() }))
                .collect();
            let response = json!({ "events": matching, "cursor": self.event_seq, "timed_out": matching.is_empty(), "sessions": sessions });
            let _ = waiter.reply.send(ControlOutcome::Done(ControlResponse::ok(response)));
        }
        self.waiters = pending;
    }

    /// Status for agents without hooks, read from what their terminal shows.
    fn track_busy(&mut self) {
        if self.busy_checked.elapsed() < Duration::from_millis(400) {
            return;
        }
        self.busy_checked = Instant::now();
        let mut finished = Vec::new();
        for session in &mut self.sessions {
            let Some(busy_text) = self.config.agent(&session.agent).and_then(|a| a.busy_text.clone()) else { continue };
            let Some(terminal) = &session.terminal else { continue };
            if session.hooks_active || terminal.exited() || session.status == AgentStatus::NeedsInput {
                continue;
            }
            let busy = terminal.screen_text(12).contains(&busy_text);
            session.status = match (busy, session.status) {
                (true, _) => AgentStatus::Working,
                (false, AgentStatus::Working) => {
                    finished.push(session.root());
                    AgentStatus::Done
                }
                (false, AgentStatus::Starting) => AgentStatus::Idle,
                (false, other) => other,
            };
        }
        for root in finished {
            self.diffs.entry(root).or_default().mark_dirty();
        }
    }

    fn close(&mut self, id: &str) {
        if let Some(i) = self.sessions.iter().position(|s| s.id == id) {
            let session = self.sessions.remove(i);
            if session.terminal.is_none()
                && let Some(cid) = session.claude_id
            {
                self.dismissed.insert(cid);
            }
        }
        if self.active.as_deref() == Some(id) {
            self.active = self.sessions.first().map(|s| s.id.clone());
        }
    }

    fn remember(&mut self, id: &str) {
        let Some(s) = self.sessions.iter().find(|s| s.id == id) else { return };
        let Some(claude_id) = s.claude_id.clone() else { return };
        self.store.upsert(SessionRecord {
            claude_session_id: claude_id,
            agent: s.agent.clone(),
            cwd: s.cwd.clone(),
            title: s.title.clone(),
            last_prompt: s.last_prompt.clone(),
            updated: 0,
        });
        if let Err(err) = self.store.save(&paths::store_file()) {
            eprintln!("IMACS: saving sessions failed: {err:#}");
        }
    }

    /// Sends text to a session: typed into IMACS's own terminals, queued for the
    /// next hook of sessions running elsewhere.
    fn deliver(&mut self, index: usize, text: &str) -> Delivery {
        let session = &mut self.sessions[index];
        if session.approval_hold || !session.approvals.is_empty() {
            session.held.push(text.to_string());
            return Delivery::Held;
        }
        if let Some(terminal) = &session.terminal {
            if terminal.exited() {
                return Delivery::Unavailable;
            }
            terminal.submit(text);
            let trackable = session.kind == AgentKind::Claude
                || self.config.agent(&session.agent).is_some_and(|a| a.busy_text.is_some());
            if trackable {
                let was_working = session.status == AgentStatus::Working;
                session.pending =
                    Some(Pending { text: text.to_string(), sent: Instant::now(), attempts: 1, was_working });
            } else if session.is_agent() {
                session.status = AgentStatus::Working;
            }
            return Delivery::Sent;
        }
        let Some(claude_id) = session.claude_id.clone() else { return Delivery::Unavailable };
        self.hub.lock().unwrap().outbox.entry(claude_id).or_default().push(text.to_string());
        Delivery::Queued
    }

    fn steer_targets(&self) -> Vec<usize> {
        let indices = 0..self.sessions.len();
        match self.steer_target {
            SteerTarget::Active => self.active_index().into_iter().collect::<Vec<usize>>(),
            SteerTarget::Agents => {
                indices.filter(|&i| self.sessions[i].is_agent() && !self.sessions[i].chief).collect::<Vec<usize>>()
            }
            SteerTarget::Selected => indices.filter(|&i| self.sessions[i].selected).collect::<Vec<usize>>(),
        }
        .into_iter()
        .filter(|&i| self.sessions[i].can_receive())
        .collect()
    }

    fn send_steer(&mut self) {
        let text = self.steer_text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let targets = self.steer_targets();
        if targets.is_empty() {
            self.toast("No session to send to");
            return;
        }
        let mut typed = 0;
        let mut queued = 0;
        let mut held = 0;
        for i in targets {
            match self.deliver(i, &text) {
                Delivery::Sent => typed += 1,
                Delivery::Queued => queued += 1,
                Delivery::Held => held += 1,
                Delivery::Unavailable => {}
            }
        }
        self.steer_text.clear();
        let mut parts = Vec::new();
        if typed > 0 {
            parts.push(format!("Sent to {typed} session{}", if typed == 1 { "" } else { "s" }));
        }
        if queued > 0 {
            parts.push(format!("Queued for {queued} outside IMACS"));
        }
        if held > 0 {
            parts.push(format!("Held for {held} until permission is answered"));
        }
        let summary = if parts.is_empty() { "No session could receive the message".into() } else { parts.join(", ") };
        self.toast(summary);
    }

    fn send_review(&mut self, root: &Path) -> Delivery {
        let comments = self.diffs.get(root).map(|s| s.comments.clone()).unwrap_or_default();
        if comments.is_empty() {
            return Delivery::Unavailable;
        }
        let header = self.config.fill(&self.config.steer.review_header);
        let message = steer::compose_review(&header, &comments);
        let target = self
            .active_index()
            .filter(|&i| self.sessions[i].root() == root)
            .or_else(|| self.sessions.iter().position(|s| s.root() == root && s.is_agent() && s.can_receive()));
        let Some(index) = target else {
            self.toast("No agent is working in this worktree");
            return Delivery::Unavailable;
        };
        let count = comments.len();
        let delivery = self.deliver(index, &message);
        if delivery != Delivery::Unavailable {
            if let Some(state) = self.diffs.get_mut(root) {
                state.comments.clear();
            }
            let title = self.sessions[index].title.clone();
            let action = if delivery == Delivery::Held { "Held" } else { "Sent" };
            self.toast(format!("{action} {count} comment{} to {title}", if count == 1 { "" } else { "s" }));
        }
        delivery
    }

    // ── background events ─────────────────────────────────────────────────

    fn pump(&mut self, ctx: &egui::Context) {
        self.usage.drain();
        for (agent, dir) in std::mem::take(&mut self.pending_open) {
            self.spawn(ctx, &agent, dir, None);
        }
        let pending = std::mem::take(&mut self.pending_codex);
        for (launch, ready) in pending {
            if self.find(&launch.id).is_none() {
                continue;
            }
            if let Some(supported) = ready.get().copied() {
                let id = launch.id.clone();
                if let Err(error) = self.launch_agent(ctx, launch, Some(supported)) {
                    if let Some(index) = self.find(&id) {
                        self.sessions[index].status = AgentStatus::Exited;
                        self.sessions[index].last_message = Some(error.clone());
                    }
                    self.toast(error);
                }
            } else {
                self.pending_codex.push((launch, ready));
            }
        }
        while let Ok(message) = self.hook_rx.try_recv() {
            self.on_hook(message);
        }
        while let Ok((branch, result, launch)) = self.worktree_rx.try_recv() {
            self.worktrees_at = None;
            if let Some(i) = self.creating.iter().position(|b| *b == branch) {
                self.creating.remove(i);
            }
            let background = launch
                .task
                .as_ref()
                .is_some_and(|(plan, _)| !self.plan_view || self.plans[self.selected_plan].id != *plan);
            let selection = (self.active.clone(), self.diff_root.clone(), self.focus_terminal.clone());
            let outcome =
                result.and_then(|path| self.start(ctx, &launch.agent, path, None, launch.args, launch.prompt));
            if background {
                (self.active, self.diff_root, self.focus_terminal) = selection;
            }
            if let Some((plan_id, task_id)) = &launch.task
                && let Some(index) = self.plans.iter().position(|p| &p.id == plan_id)
                && let Some(task) = self.plans[index].task_mut(task_id)
            {
                match &outcome {
                    Ok(id) => task.session = Some(id.clone()),
                    Err(err) => {
                        task.set_status(TaskStatus::Blocked);
                        task.notes.push(format!("could not start: {err}"));
                    }
                }
                self.save_plan(index);
            }
            let response = match outcome {
                Ok(id) => ControlResponse::ok(self.describe(self.find(&id).expect("just started"))),
                Err(err) => {
                    self.toast(err.clone());
                    ControlResponse::error(err)
                }
            };
            if let Some(reply) = launch.reply {
                let _ = reply.send(ControlOutcome::Done(response));
            }
        }
        while let Ok(integrated) = self.integrate_rx.try_recv() {
            self.finish_integration(integrated);
        }
        self.expire_approvals(Instant::now());
        if self.sessions.iter().any(|session| session.approval_hold || !session.approvals.is_empty()) {
            ctx.request_repaint_after(Duration::from_millis(400));
        }
        while let Ok(message) = self.control_rx.try_recv() {
            self.control(ctx, message.request, message.reply);
        }
        while let Ok(event) = self.voice_rx.try_recv() {
            match event {
                VoiceEvent::Opened(cancelled, result) => {
                    if matches!(&self.voice, Voice::Opening(current) if Arc::ptr_eq(current, &cancelled)) {
                        match result {
                            Ok(recorder) => self.voice = Voice::Recording(recorder),
                            Err(err) => {
                                self.voice = Voice::Idle;
                                self.toast(format!("Voice: {err}"));
                            }
                        }
                    }
                }
                VoiceEvent::Transcript(result) => {
                    self.voice = Voice::Idle;
                    match result {
                        Ok(text) => self.send_voice(ctx, &text),
                        Err(err) => self.toast(format!("Voice: {err}")),
                    }
                }
            }
        }
        if let Voice::Recording(recorder) = &self.voice {
            if recorder.elapsed() >= MAX_RECORDING {
                self.stop_voice();
                self.toast("Voice: stopped after 5 minutes, transcribing");
            } else {
                // Keeps the recording clock moving.
                ctx.request_repaint_after(Duration::from_millis(250));
            }
        }
        self.track_busy();
        self.verify_deliveries();
        self.record_events();
        self.sync_plan();
        // Events wake waiters as they come in; this only times them out.
        if let Some(deadline) = self.waiters.iter().map(|w| w.deadline).min() {
            ctx.request_repaint_after(deadline.saturating_duration_since(Instant::now()));
        }
        if self.sessions.iter().any(|s| s.status == AgentStatus::Working) {
            // Keeps the status pulse of working agents moving, in a few steps a second.
            ctx.request_repaint_after(PULSE_STEP);
        }
        while let Ok(result) = self.worker.rx.try_recv() {
            match result {
                WorkerResult::Diff { root, result } => {
                    let state = self.diffs.entry(root.clone()).or_default();
                    state.loading = false;
                    match result {
                        Ok(prepared) => {
                            state.prepared = Some(prepared);
                            state.error = None;
                        }
                        Err(err) => state.error = Some(err),
                    }
                    // A file open in the viewer changes with the worktree too.
                    if let Some(path) = state.viewing.clone() {
                        self.worker.request_file(root, path, self.highlighter.clone());
                    }
                }
                WorkerResult::File { root, path, result } => {
                    let state = self.diffs.entry(root).or_default();
                    if state.viewing.as_deref() == Some(path.as_str()) {
                        match result {
                            Ok(file) => state.file = Some(file),
                            Err(err) => state.file_error = Some(err),
                        }
                    }
                }
            }
        }
        while let Ok(root) = self.watch_rx.try_recv() {
            self.diffs.entry(root).or_default().mark_dirty();
        }
        for session in &mut self.sessions {
            if session.terminal.as_ref().is_some_and(Terminal::exited) {
                session.status = AgentStatus::Exited;
            }
            if let Some(title) = session.terminal.as_ref().and_then(|t| t.shared.title.lock().unwrap().take())
                && session.kind == AgentKind::Shell
                && !title.trim().is_empty()
            {
                session.title = title;
            }
        }

        if self.config_checked.elapsed() > Duration::from_secs(1) {
            self.config_checked = Instant::now();
            let stamp = modified(&self.config_path);
            if stamp != self.config_stamp {
                self.config_stamp = stamp;
                match Config::load(&self.config_path) {
                    Ok(config) => {
                        let syntax = Palette::from_config(&config.theme).syntax;
                        let theme_changed = syntax != self.palette.syntax;
                        if config.terminal.scrollback != self.config.terminal.scrollback {
                            let terminals =
                                self.sessions.iter().filter_map(|s| s.terminal.as_ref()).chain(self.editors.values());
                            for terminal in terminals {
                                terminal.set_scrollback(config.terminal.scrollback);
                            }
                        }
                        self.config = config;
                        self.config_error = None;
                        self.palette = Arc::new(Palette::from_config(&self.config.theme));
                        if theme_changed {
                            self.highlighter = Arc::new(Highlighter::new(&syntax));
                        }
                        apply_style(ctx, &self.config);
                        self.apply_config();
                        for state in self.diffs.values_mut() {
                            state.mark_dirty();
                        }
                        self.toast("Config reloaded");
                    }
                    Err(err) => self.config_error = Some(format!("{err:#}")),
                }
            }
        }

        if self.worktrees_at.is_none_or(|at| at.elapsed() > WORKTREE_REFRESH) {
            self.worktrees_at = Some(Instant::now());
            self.worktrees.clear();
            self.worktree_project.clear();
            for repo in self.repos.clone() {
                for worktree in git::list_worktrees(&repo).unwrap_or_default() {
                    self.worktree_project.insert(worktree.path.clone(), repo.clone());
                    self.worktrees.push(worktree);
                }
                // The explorer lists the main checkout's files.
                let state = self.diffs.entry(repo).or_default();
                if !state.loading {
                    state.mark_dirty();
                }
            }
        }
        let names: Vec<String> =
            self.repos.iter().filter_map(|r| r.file_name()).map(|n| n.to_string_lossy().into_owned()).collect();
        let title = match &self.workspace {
            Some(ws) => format!("IMACS · {ws} · {}", names.join(", ")),
            None if names.is_empty() => "IMACS".to_string(),
            None => format!("IMACS · {}", names.join(", ")),
        };
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }

        let target = self.diff_target();
        self.watch(ctx, target.as_deref());
        if let Some(root) = &target {
            let state = self.diffs.entry(root.clone()).or_default();
            // A diff seen off screen may lack untracked content; on screen it gets all of it.
            let missing = state.prepared.as_ref().is_none_or(|p| p.diff.partial);
            if missing && !state.loading && state.error.is_none() && state.dirty_since.is_none() {
                state.dirty_since = Some(Instant::now() - Duration::from_secs(1));
            }
        }
        // File changes and hooks keep the diffs of agents off screen current; this
        // catches what they miss while an agent works.
        for session in &self.sessions {
            let state = self.diffs.entry(session.root()).or_default();
            let stale = state.last_request.is_none_or(|at| at.elapsed() > FALLBACK_REFRESH);
            if (session.status == AgentStatus::Working || state.prepared.is_none()) && stale && !state.loading {
                state.mark_dirty();
            }
        }
        let now = Instant::now();
        let mut next: Option<Instant> = None;
        for (root, state) in &mut self.diffs {
            // The diff on screen follows every change; the others at most every few seconds,
            // with only some of their untracked content.
            let on_screen = target.as_ref() == Some(root);
            let (interval, budget) =
                if on_screen { (Duration::ZERO, None) } else { (BACKGROUND_REFRESH, Some(OFF_SCREEN_UNTRACKED)) };
            match state.due_at(interval) {
                Some(at) if at <= now => {
                    state.requested();
                    let (base_ref, highlighter) = (self.config.base_ref.clone(), self.highlighter.clone());
                    self.worker.request(root.clone(), base_ref, budget, highlighter);
                }
                Some(at) => next = Some(next.map_or(at, |next| next.min(at))),
                None => {}
            }
        }
        if let Some(at) = next {
            ctx.request_repaint_after(at - now);
        }
        if let Some((_, at)) = &self.toast
            && at.elapsed() > Duration::from_secs(5)
        {
            self.toast = None;
        }
    }

    fn on_hook(&mut self, message: HookMessage) {
        let event = message.event;
        if self.dismissed.contains(&event.session_id) {
            return;
        }
        let found = message
            .pane
            .as_ref()
            .and_then(|pane| self.sessions.iter().position(|s| &s.id == pane))
            .or_else(|| self.sessions.iter().position(|s| s.claude_id.as_deref() == Some(event.session_id.as_str())))
            .or_else(|| {
                message.ancestors.iter().find_map(|pid| {
                    self.sessions
                        .iter()
                        .position(|s| s.terminal.as_ref().is_some_and(|terminal| terminal.process_id() == *pid))
                })
            });
        let index = match found {
            Some(index) => index,
            None if event.name == "SessionEnd" => return,
            None => {
                let cwd = event.cwd.clone().unwrap_or_else(paths::home);
                let worktree = git::worktree_of(&cwd);
                let title = title_for(worktree.as_ref().and_then(|w| w.branch.as_deref()), &cwd);
                self.sessions.push(Session {
                    id: format!("ext:{}", event.session_id),
                    agent: "claude".into(),
                    kind: AgentKind::Claude,
                    terminal: None,
                    claude_id: Some(event.session_id.clone()),
                    cwd,
                    worktree,
                    title,
                    status: AgentStatus::Idle,
                    last_prompt: None,
                    last_message: None,
                    hooks_active: false,
                    approvals: Vec::new(),
                    allowed: Vec::new(),
                    held: Vec::new(),
                    approval_hold: false,
                    menu_absent_since: None,
                    selected: false,
                    chief: false,
                    pending: None,
                });
                if self.active.is_none() {
                    self.active = Some(format!("ext:{}", event.session_id));
                }
                self.sessions.len() - 1
            }
        };

        let session = &mut self.sessions[index];
        session.claude_id = Some(event.session_id.clone());
        session.hooks_active = true;
        if event.name == "PermissionRequest" {
            let request = ApprovalRequest::from_event(&event);
            if (self.config.approvals.allows(&request, &session.root())
                || session.allowed.iter().any(|allowed| allowed.same_command(&request)))
                && message
                    .approval_reply
                    .as_ref()
                    .is_some_and(|reply| reply.answer(HookReply { stdout: Some(Decision::Allow.output(None)) }))
            {
                return;
            }
            session.approval_hold = true;
            session.menu_absent_since = None;
            session.approvals.push(PendingApproval { request, reply: message.approval_reply });
            self.last_status.remove(&session.id);
        } else if event.name == "PreToolUse" {
            session.approvals.retain(|pending| pending.reply.as_ref().is_some_and(|reply| !reply.expired()));
        } else if event.name == "PostToolUse" {
            let completed = ApprovalRequest::from_event(&event);
            session.approvals.retain(|pending| !pending.request.same_command(&completed));
        } else if matches!(event.name.as_str(), "Stop" | "SessionEnd" | "Interrupt" | "UserPromptSubmit") {
            session.approvals.clear();
        }
        if let Some(cwd) = &event.cwd
            && *cwd != session.cwd
        {
            session.cwd = cwd.clone();
            session.worktree = git::worktree_of(cwd);
            session.title = title_for(session.branch(), cwd);
        }
        if let Some(status) = event.status() {
            session.status = if session.approvals.is_empty() { status } else { AgentStatus::NeedsInput };
        }
        match event.name.as_str() {
            "UserPromptSubmit" => session.last_prompt = event.prompt.clone(),
            "Stop" => session.last_message = event.last_assistant_message.clone(),
            _ => {}
        }
        let root = session.root();
        let (id, title) = (session.id.clone(), session.title.clone());
        if matches!(event.name.as_str(), "PostToolUse" | "Stop" | "UserPromptSubmit" | "CwdChanged")
            && let Some(state) = self.diffs.get_mut(&root)
        {
            state.mark_dirty();
        }
        if message.delivered > 0 {
            self.toast(format!("Delivered {} queued message(s) to {title}", message.delivered));
        }
        if matches!(event.name.as_str(), "SessionStart" | "UserPromptSubmit" | "Stop") {
            self.remember(&id);
        }
        if matches!(event.name.as_str(), "PostToolUse" | "Stop" | "Interrupt" | "UserPromptSubmit") {
            let session = &mut self.sessions[index];
            if session.approvals.is_empty() && session.approval_hold {
                let menu = session
                    .terminal
                    .as_ref()
                    .is_some_and(|terminal| imacs_core::approvals::menu_visible(&terminal.screen_text(60)));
                session.approval_hold = menu;
                session.menu_absent_since = None;
            }
            self.release_held(index);
        }
        if event.name == "PermissionRequest" {
            self.last_status.insert(id.clone(), AgentStatus::NeedsInput);
            self.event_seq += 1;
            self.events.push(StatusEvent {
                seq: self.event_seq,
                plan: self.plans.iter().find(|p| p.contains_session(&id)).map(|p| p.id.clone()),
                task: self
                    .plans
                    .iter()
                    .flat_map(|p| &p.tasks)
                    .find(|t| t.session.as_deref() == Some(&id))
                    .map(|t| t.id.clone()),
                session: id,
                title,
                status: AgentStatus::NeedsInput,
                at: imacs_core::store::now(),
                report: None,
                approval: self.sessions[index].approvals.last().map(|pending| pending.request.clone()),
            });
            self.serve_waiters();
        }
    }

    fn release_held(&mut self, index: usize) {
        if !self.sessions[index].approval_hold
            && self.sessions[index].approvals.is_empty()
            && !self.sessions[index].held.is_empty()
        {
            let text = std::mem::take(&mut self.sessions[index].held).join("\n\n");
            self.deliver(index, &text);
        }
    }

    fn expire_approvals(&mut self, now: Instant) {
        if now.duration_since(self.approvals_checked) < APPROVAL_CHECK_INTERVAL {
            return;
        }
        self.approvals_checked = now;
        for index in 0..self.sessions.len() {
            let session = &mut self.sessions[index];
            if !session.approval_hold && session.approvals.is_empty() {
                continue;
            }
            if session.approvals.iter().any(|pending| pending.reply.as_ref().is_some_and(|reply| !reply.expired())) {
                session.menu_absent_since = None;
                continue;
            }
            let menu = session
                .terminal
                .as_ref()
                .is_some_and(|terminal| imacs_core::approvals::menu_visible(&terminal.screen_text(60)));
            if menu {
                session.approval_hold = true;
                session.menu_absent_since = None;
                continue;
            }
            let missing_since = session.menu_absent_since.get_or_insert(now);
            if now.duration_since(*missing_since) < APPROVAL_MENU_GRACE {
                continue;
            }
            session.approvals.clear();
            session.approval_hold = false;
            session.menu_absent_since = None;
            if session.status == AgentStatus::NeedsInput {
                session.status = if session.terminal.as_ref().is_some_and(Terminal::exited) {
                    AgentStatus::Exited
                } else {
                    AgentStatus::Working
                };
            }
            self.release_held(index);
        }
    }

    fn approve(&mut self, index: usize, decision: Decision, message: Option<&str>) -> Result<(), String> {
        self.expire_approvals(Instant::now());
        let session = &mut self.sessions[index];
        let Some(pending) = session.approvals.first_mut() else { return Err("no pending permission request".into()) };
        let reply = HookReply { stdout: Some(decision.output(message)) };
        let answered = pending.reply.take().is_some_and(|sender| sender.answer(reply));
        if !answered {
            let Some(terminal) = &session.terminal else {
                return Err("the permission hook has returned; answer in the agent terminal".into());
            };
            let Some(key) = imacs_core::approvals::menu_key(
                &terminal.screen_text(60),
                pending.request.command.as_deref(),
                decision,
            ) else {
                return Err("the permission menu is not visible; retry or answer in the terminal".into());
            };
            terminal.write(key.into_bytes());
        }
        let pending = session.approvals.remove(0);
        if decision == Decision::AllowAlways {
            session.allowed.push(pending.request);
        }
        session.status = if session.approvals.is_empty() { AgentStatus::Working } else { AgentStatus::NeedsInput };
        Ok(())
    }

    /// Watches the worktree of every agent and the one on screen, so their diffs follow
    /// file changes without polling.
    fn watch(&mut self, ctx: &egui::Context, target: Option<&Path>) {
        let mut wanted: Vec<PathBuf> = self
            .sessions
            .iter()
            .filter(|s| s.worktree.is_some())
            .map(Session::root)
            .chain(target.map(Path::to_path_buf))
            .collect();
        wanted.sort();
        wanted.dedup();
        let current: Vec<PathBuf> = self.watched.lock().unwrap().iter().map(|(root, _)| root.clone()).collect();
        if current == wanted {
            return;
        }
        if self.watcher.is_none() {
            let (tx, ctx, watched) = (self.watch_tx.clone(), ctx.clone(), self.watched.clone());
            let handler = move |result: DebounceEventResult| {
                let Ok(events) = result else { return };
                let watched = watched.lock().unwrap();
                let mut changed: Vec<&PathBuf> = Vec::new();
                for event in &events {
                    // The innermost worktree, for worktrees inside another.
                    let Some((root, relative)) = watched
                        .iter()
                        .filter_map(|(root, real)| Some((root, event.path.strip_prefix(real).ok()?)))
                        .min_by_key(|(_, relative)| relative.components().count())
                    else {
                        continue;
                    };
                    let ignored = relative.components().any(|c| IGNORED_DIRS.iter().any(|d| c.as_os_str() == *d));
                    if !ignored && !changed.contains(&root) {
                        changed.push(root);
                    }
                }
                for root in &changed {
                    let _ = tx.send((*root).clone());
                }
                if !changed.is_empty() {
                    ctx.request_repaint();
                }
            };
            match new_debouncer(Duration::from_millis(250), handler) {
                Ok(debouncer) => self.watcher = Some(debouncer),
                Err(err) => eprintln!("IMACS: file watcher failed: {err}"),
            }
        }
        if let Some(debouncer) = &mut self.watcher {
            let watcher = debouncer.watcher();
            for root in current.iter().filter(|root| !wanted.contains(root)) {
                let _ = watcher.unwatch(root);
            }
            for root in wanted.iter().filter(|root| !current.contains(root)) {
                let _ = watcher.watch(root, RecursiveMode::Recursive);
            }
        }
        // Events carry the path as the file system has it (/private/tmp for /tmp).
        *self.watched.lock().unwrap() =
            wanted.into_iter().map(|root| (root.clone(), std::fs::canonicalize(&root).unwrap_or(root))).collect();
    }

    // ── input ─────────────────────────────────────────────────────────────

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let keys = self.config.keys.clone();
        let pressed = |text: &str| shortcut(text).is_some_and(|s| ctx.input_mut(|i| i.consume_shortcut(&s)));
        if pressed(&keys.voice) {
            self.toggle_voice();
        }
        if matches!(self.voice, Voice::Opening(_) | Voice::Recording(_))
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape))
        {
            if let Voice::Opening(cancelled) = &self.voice {
                cancelled.store(true, Ordering::Relaxed);
            }
            self.voice = Voice::Idle;
            self.toast("Voice: recording cancelled");
        }
        if pressed(&keys.toggle_diff) {
            self.diff_open = !self.diff_open;
        }
        if pressed(&keys.focus_steer) {
            self.focus_steer = true;
        }
        if pressed(&keys.toggle_editor)
            && let Some(root) = self.diff_target()
        {
            if self.panel == PanelMode::Editor {
                self.panel = PanelMode::Diff;
            } else {
                let state = self.diffs.get(&root);
                let file = state.and_then(|s| s.viewing.clone()).or_else(|| {
                    state
                        .and_then(|s| s.prepared.as_ref())
                        .and_then(|p| p.diff.files.first().map(|f| f.path().to_string()))
                });
                self.edit(ctx, &root, file.as_deref(), None);
            }
        }
        // Shift+↓/↑ walk the changed files from anywhere but a text field or the editor.
        let editor_focused = self.editor_id.is_some_and(|id| ctx.memory(|m| m.has_focus(id)));
        if !ctx.text_edit_focused() && !editor_focused {
            let step = ctx.input_mut(|i| {
                if i.consume_key(egui::Modifiers::SHIFT, Key::ArrowDown) {
                    1
                } else if i.consume_key(egui::Modifiers::SHIFT, Key::ArrowUp) {
                    -1
                } else {
                    0
                }
            });
            if step != 0
                && let Some(root) = self.diff_target()
            {
                self.diff_open = true;
                if self.panel == PanelMode::Plan {
                    self.panel = PanelMode::Diff;
                }
                let moved = self.diffs.entry(root.clone()).or_default().step(step);
                if self.panel == PanelMode::Editor
                    && let Some((path, line)) = moved
                {
                    self.edit(ctx, &root, Some(&path), line);
                    self.focus_editor = false;
                }
            }
        }
        if pressed(&keys.toggle_plan) {
            self.toggle_plan();
        }
        if pressed(&keys.toggle_chief) {
            self.toggle_chief(ctx);
        }
        if pressed(&keys.quick_open) && self.diff_target().is_some() {
            self.quick_open = Some(QuickOpen { query: String::new(), selected: 0, focus: true });
        }
        if pressed(&keys.new_session) {
            self.open_new_dialog(None);
        }
        if pressed(&keys.refresh_diff)
            && let Some(root) = self.diff_target()
        {
            let state = self.diffs.entry(root).or_default();
            state.dirty_since = Some(Instant::now() - Duration::from_secs(1));
        }
        if pressed(&keys.next_attention) {
            let start = self.active_index().map(|i| i + 1).unwrap_or(0);
            let n = self.sessions.len();
            if let Some(i) = (0..n).map(|k| (start + k) % n).find(|&i| self.sessions[i].status.needs_attention()) {
                let id = self.sessions[i].id.clone();
                self.activate(&id, true);
            } else {
                self.toast("Nothing needs you right now");
            }
        }
        if pressed(&keys.close_session)
            && let Some(id) = self.active.clone()
        {
            self.confirm_close = Some(id);
        }
        for (n, key) in
            [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9]
                .into_iter()
                .enumerate()
        {
            let chord = egui::KeyboardShortcut::new(shortcut("cmd+a").map(|s| s.modifiers).unwrap_or_default(), key);
            // cmd+N jumps to the Nth agent on screen and loads its diff.
            let visible = self.visible_sessions();
            if ctx.input_mut(|i| i.consume_shortcut(&chord))
                && let Some(id) = visible.get(n).map(|&i| self.sessions[i].id.clone())
            {
                self.activate(&id, true);
                if self.panel == PanelMode::Plan {
                    self.panel = PanelMode::Diff;
                }
            }
        }
    }

    fn open_new_dialog(&mut self, folder: Option<PathBuf>) {
        let folder = folder.or_else(|| self.diff_target()).unwrap_or_else(|| self.launch_dir.clone());
        let repo = self.repos.first().cloned().unwrap_or_else(|| folder.clone());
        self.new_dialog = Some(NewDialog {
            agent: self.config.agents.first().map(|a| a.name.clone()).unwrap_or_default(),
            new_worktree: false,
            folder: paths::display(&folder),
            repo: paths::display(&repo),
            branch: String::new(),
            error: None,
        });
    }

    // ── drawing ───────────────────────────────────────────────────────────

    fn metrics(&self, ui: &egui::Ui) -> Metrics {
        let font = FontId::monospace(self.config.font.size);
        let cell = ui.fonts_mut(|f| vec2(f.glyph_width(&font, 'M'), f.row_height(&font)));
        Metrics { font, cell: vec2(cell.x, cell.y.ceil()) }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let p = self.palette.clone();
        ui.horizontal_centered(|ui| {
            ui.image((self.icon.id(), vec2(20.0, 20.0)));
            ui.label(RichText::new("IMACS").strong().size(14.0).color(p.accent).monospace());
            let agents = self.sessions.iter().filter(|s| s.is_agent()).count();
            let waiting = self.sessions.iter().filter(|s| s.status.needs_attention()).count();
            ui.label(RichText::new(format!("{agents} agent{}", if agents == 1 { "" } else { "s" })).color(p.muted));
            ui.separator();
            self.view_tabs(ui);
            if waiting > 0 {
                let label = ui.add(
                    egui::Label::new(
                        RichText::new(format!("● {waiting} need{} you", if waiting == 1 { "s" } else { "" }))
                            .color(p.warning),
                    )
                    .sense(Sense::click()),
                );
                if label.on_hover_text(format!("Jump with {}", self.config.keys.next_attention)).clicked()
                    && let Some(i) = self.sessions.iter().position(|s| s.status.needs_attention())
                {
                    let id = self.sessions[i].id.clone();
                    self.activate(&id, true);
                }
            }
            if let Some((text, _)) = &self.toast {
                ui.separator();
                ui.label(RichText::new(text).color(p.foreground));
            }
            if let Some(err) = &self.config_error {
                ui.label(RichText::new("config error").color(p.error)).on_hover_text(err);
            }
            if let Some(err) = &self.hook_error {
                ui.label(RichText::new("hooks offline").color(p.error)).on_hover_text(err);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button("Settings", |ui| self.settings_menu(ui));
                ui.menu_button("Project", |ui| self.project_menu(ui));
                let (done, total) = (
                    self.plans[self.selected_plan].count(TaskStatus::Done),
                    self.plans[self.selected_plan].tasks.len(),
                );
                let label = if total == 0 { "Plan".to_string() } else { format!("Plan {done}/{total}") };
                let attention = self.plans[self.selected_plan].count(TaskStatus::Review)
                    + self.plans[self.selected_plan].count(TaskStatus::Blocked);
                let text = RichText::new(label).color(if attention > 0 { p.warning } else { p.foreground });
                let open = self.panel == PanelMode::Plan && self.diff_open;
                if ui.selectable_label(open, text).on_hover_text(&self.config.keys.toggle_plan).clicked() {
                    self.toggle_plan();
                }
                if ui.button("+ New").on_hover_text(&self.config.keys.new_session).clicked() {
                    self.open_new_dialog(None);
                }
                let diff_label = if self.diff_open { "Hide diff" } else { "Show diff" };
                if ui.button(diff_label).on_hover_text(&self.config.keys.toggle_diff).clicked() {
                    self.diff_open = !self.diff_open;
                }
            });
        });
    }

    fn steer_bar(&mut self, ui: &mut egui::Ui) {
        let p = self.palette.clone();
        ui.horizontal(|ui| {
            let active_title =
                self.active_index().map(|i| self.sessions[i].title.clone()).unwrap_or_else(|| "none".into());
            let agents = self.sessions.iter().filter(|s| s.is_agent() && s.can_receive()).count();
            let selected = self.sessions.iter().filter(|s| s.selected).count();
            let label = match self.steer_target {
                SteerTarget::Active => format!("→ {active_title}"),
                SteerTarget::Agents => format!("→ all agents ({agents})"),
                SteerTarget::Selected => format!("→ selected ({selected})"),
            };
            egui::ComboBox::from_id_salt("steer-target").selected_text(label).width(190.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.steer_target, SteerTarget::Active, format!("Active: {active_title}"));
                ui.selectable_value(&mut self.steer_target, SteerTarget::Agents, format!("All agents ({agents})"));
                ui.selectable_value(&mut self.steer_target, SteerTarget::Selected, format!("Selected ({selected})"));
            });

            let id = ui.scope_id().with("steer-input");
            let focused = ui.memory(|m| m.has_focus(id));
            let mut send = false;
            if focused && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter)) {
                send = true;
            }
            let width = ui.available_width() - 70.0;
            let edit = egui::TextEdit::multiline(&mut self.steer_text)
                .id(id)
                .desired_rows(1)
                .desired_width(width)
                .hint_text(format!(
                    "Steer · Enter sends · Shift+Enter newline · {} focuses",
                    self.config.keys.focus_steer
                ))
                .font(p.ui(13.5));
            let response = ui.add(edit);
            if std::mem::take(&mut self.focus_steer) {
                response.request_focus();
            }
            let button =
                egui::Button::new(RichText::new("Send").color(Color32::WHITE)).fill(p.accent.gamma_multiply(0.6));
            if ui.add(button).clicked() {
                send = true;
            }
            if send {
                self.send_steer();
            }
        });
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let p = self.palette.clone();
        let files_root = self.diff_target();
        section(ui, &p, "USAGE");
        self.usage.show(ui, &p);
        ui.separator();
        let total = ui.available_height();
        let sessions_height = if files_root.is_some() { (total * 0.46).max(150.0) } else { total };
        ui.add_space(6.0);
        egui::ScrollArea::vertical().id_salt("sidebar").max_height(sessions_height).auto_shrink([false, true]).show(ui, |ui| {
            section(ui, &p, "AGENTS");
            let mut clicked: Option<String> = None;
            let mut close: Option<String> = None;
            let visible = self.visible_sessions();
            for (index, session) in self.sessions.iter_mut().enumerate() {
                if !visible.contains(&index) {
                    continue;
                }
                let active = self.active.as_deref() == Some(session.id.as_str());
                let stats = self.diffs.get(&session.root()).and_then(|d| d.prepared.as_ref()).map(|p| (p.additions, p.deletions));
                let row = session_row(ui, &p, session, active, stats);
                if row.clicked() {
                    clicked = Some(session.id.clone());
                }
                row.context_menu(|ui| {
                    ui.checkbox(&mut session.selected, "Include in “selected”");
                    if ui.button("Close").clicked() {
                        close = Some(session.id.clone());
                        ui.close();
                    }
                });
            }
            for branch in &self.creating {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new(format!("checking out {branch}…")).color(p.muted).size(12.0));
                });
            }
            if self.sessions.is_empty() && self.creating.is_empty() {
                ui.label(RichText::new("No agents yet. Start one with + New,\nor install hooks to see agents\nrunning in other terminals.").color(p.muted).size(12.0));
            }
            if let Some(id) = clicked {
                self.activate(&id, true);
            }
            if let Some(id) = close {
                self.confirm_close = Some(id);
            }

            let live_ids: HashSet<&str> = self.sessions.iter().filter_map(|s| s.claude_id.as_deref()).collect();
            let recent: Vec<SessionRecord> = self
                .store
                .sessions
                .iter()
                .filter(|r| !live_ids.contains(r.claude_session_id.as_str()) && r.cwd.is_dir() && r.agent != "chief")
                .filter(|r| self.project_of(&r.cwd).is_some())
                .filter({
                    let mut seen = HashSet::new();
                    move |r| seen.insert((r.title.clone(), r.cwd.clone()))
                })
                .take(8)
                .cloned()
                .collect();
            if !recent.is_empty() {
                ui.add_space(10.0);
                section(ui, &p, "RECENT · click to resume");
                for record in recent {
                    let hover = record.last_prompt.clone().unwrap_or_else(|| paths::display(&record.cwd));
                    let response = ui.selectable_label(false, RichText::new(&record.title).color(p.muted).size(12.5)).on_hover_text(hover);
                    if response.clicked() {
                        self.pending_open_resume(record);
                    }
                }
            }

        });

        ui.add_space(4.0);
        ui.separator();
        self.explorer(ui, &p);
    }

    /// The solution explorer: a switcher over the open projects, the focused
    /// project's worktrees, and its main checkout's files with the changes of the
    /// worktree under review laid over them.
    fn explorer(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let target = self.diff_target();
        let focus = target.as_deref().and_then(|t| self.project_of(t)).or_else(|| self.repos.first().cloned());
        ui.horizontal(|ui| {
            section(ui, p, "EXPLORER");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("+").on_hover_text("Open project…").clicked() {
                    self.pick_project();
                }
            });
        });
        let mut switch_to: Option<PathBuf> = None;
        let mut close: Option<PathBuf> = None;
        ui.horizontal_wrapped(|ui| {
            for root in self.repos.clone() {
                let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let agents =
                    self.sessions.iter().filter(|s| self.project_of(&s.root()).as_ref() == Some(&root)).count();
                let label = if agents > 0 { format!("{name} ·{agents}") } else { name };
                let selected = focus.as_ref() == Some(&root);
                let color = if selected { p.accent } else { p.foreground };
                let response = ui.selectable_label(selected, RichText::new(label).color(color).size(12.5));
                if response.clicked() && !selected {
                    switch_to = Some(root.clone());
                }
                response.on_hover_text(paths::display(&root)).context_menu(|ui| {
                    for agent in self.config.agents.clone() {
                        if ui.button(format!("Start {} in the main checkout", agent.name)).clicked() {
                            self.pending_open.push((agent.name.clone(), root.clone()));
                            ui.close();
                        }
                    }
                    if ui.button("New worktree…").clicked() {
                        self.open_new_dialog(Some(root.clone()));
                        if let Some(dialog) = &mut self.new_dialog {
                            dialog.new_worktree = true;
                            dialog.repo = paths::display(&root);
                        }
                        ui.close();
                    }
                    if ui.button("Close project").clicked() {
                        close = Some(root.clone());
                        ui.close();
                    }
                });
            }
        });
        if let Some(root) = switch_to {
            self.active = None;
            self.diff_root = Some(root);
            self.diff_open = true;
        }
        if let Some(root) = close {
            self.close_project(&root);
            return;
        }
        let Some(project) = focus else {
            ui.add_space(8.0);
            ui.label(RichText::new("Open a project with + or Project › Open project…").color(p.muted).size(12.0));
            return;
        };

        // Worktrees of the focused project, folded by default (big repos have dozens).
        let worktrees: Vec<WorktreeEntry> = self
            .worktrees
            .iter()
            .filter(|w| w.path != project && self.worktree_project.get(&w.path) == Some(&project))
            .cloned()
            .collect();
        let explorer = self.explorers.entry(project.clone()).or_default();
        let arrow = if explorer.show_worktrees { "▾" } else { "▸" };
        let mut open_cleanup = false;
        ui.horizontal(|ui| {
            let header = ui.add(
                egui::Label::new(
                    RichText::new(format!("{arrow} worktrees ({})", worktrees.len())).color(p.muted).size(12.0),
                )
                .sense(Sense::click()),
            );
            if header.clicked() {
                explorer.show_worktrees = !explorer.show_worktrees;
            }
            if ui
                .small_button("clean up…")
                .on_hover_text("Find merged or abandoned worktrees and remove them")
                .clicked()
            {
                open_cleanup = true;
            }
        });
        if open_cleanup {
            self.open_cleanup(project.clone(), None);
            return;
        }
        let explorer = self.explorers.entry(project.clone()).or_default();
        if explorer.show_worktrees {
            let mut pick: Option<PathBuf> = None;
            let mut remove_one: Option<PathBuf> = None;
            egui::ScrollArea::vertical().id_salt(("worktrees", &project)).max_height(190.0).show(ui, |ui| {
                let main_selected = target.as_ref() == Some(&project);
                if ui.selectable_label(main_selected, RichText::new("main checkout").size(12.0)).clicked() {
                    pick = Some(project.clone());
                }
                for wt in &worktrees {
                    let name = wt.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let agent = self.sessions.iter().find(|s| s.root() == wt.path);
                    let selected = target.as_ref() == Some(&wt.path);
                    let response = ui
                        .horizontal(|ui| {
                            if let Some(agent) = agent {
                                ui.label(RichText::new("●").color(status_color(agent.status, p)).size(10.0));
                            }
                            let response = ui.selectable_label(selected, RichText::new(&name).size(12.0));
                            if let Some(branch) = &wt.branch {
                                ui.label(RichText::new(branch).color(p.muted).size(10.5));
                            }
                            response
                        })
                        .inner;
                    if response.clicked() {
                        pick = Some(wt.path.clone());
                    }
                    response.context_menu(|ui| {
                        for agent in self.config.agents.clone() {
                            if ui.button(format!("Start {} here", agent.name)).clicked() {
                                self.pending_open.push((agent.name.clone(), wt.path.clone()));
                                ui.close();
                            }
                        }
                        if ui.button("Remove worktree…").clicked() {
                            remove_one = Some(wt.path.clone());
                            ui.close();
                        }
                    });
                }
            });
            if let Some(path) = remove_one {
                self.open_cleanup(project.clone(), Some(path));
                return;
            }
            if let Some(path) = pick {
                match self.sessions.iter().find(|s| s.root() == path).map(|s| s.id.clone()) {
                    Some(id) => self.activate(&id, true),
                    None => {
                        self.active = None;
                        self.diff_root = Some(path);
                        self.diff_open = true;
                    }
                }
            }
        }

        let review =
            target.filter(|t| self.project_of(t).as_ref() == Some(&project)).unwrap_or_else(|| project.clone());
        if review != project {
            let name = review.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            ui.label(RichText::new(format!("changes from {name}")).color(p.accent).size(11.0));
        }
        let main = self.diffs.get(&project).and_then(|d| d.prepared.clone());
        let changes = self.diffs.get(&review).and_then(|d| d.prepared.clone());
        let viewing = self.diffs.get(&review).and_then(|d| d.viewing.clone());
        let (main, changes) = match (main, changes) {
            (Some(main), Some(changes)) => (main, changes),
            (Some(main), None) => (main.clone(), main),
            (None, Some(changes)) => (changes.clone(), changes),
            (None, None) => {
                ui.label(RichText::new("Reading files…").color(p.muted).size(12.0));
                return;
            }
        };
        let explorer = self.explorers.entry(project).or_default();
        if let Some(path) = diff_view::files_panel(ui, explorer, &main, &changes, viewing.as_deref(), p) {
            self.open_file(&review, &path);
        }
    }

    /// Shows a file in the diff panel: its hunks when changed, the whole file otherwise.
    fn open_file(&mut self, root: &Path, path: &str) {
        if self.panel == PanelMode::Editor {
            let ctx = self.ctx.clone();
            self.edit(&ctx, root, Some(path), None);
            return;
        }
        self.diff_open = true;
        let state = self.diffs.entry(root.to_path_buf()).or_default();
        if state.open(path) {
            self.worker.request_file(root.to_path_buf(), path.to_string(), self.highlighter.clone());
        }
    }

    fn project_menu(&mut self, ui: &mut egui::Ui) {
        if ui.button("Open project…").clicked() {
            ui.close();
            self.pick_project();
        }
        let recent: Vec<PathBuf> = self
            .store
            .recent_projects
            .iter()
            .filter(|p| !self.open_projects.contains(p) && p.is_dir())
            .take(8)
            .cloned()
            .collect();
        if !recent.is_empty() {
            ui.separator();
            ui.label(RichText::new("Recent projects").size(11.0).color(self.palette.muted));
            for root in recent {
                if ui.button(paths::display(&root)).clicked() {
                    self.open_project(&root);
                    ui.close();
                }
            }
        }
        ui.separator();
        ui.label(RichText::new("Workspaces").size(11.0).color(self.palette.muted));
        for name in Workspace::list() {
            let current = self.workspace.as_deref() == Some(name.as_str());
            if ui.selectable_label(current, &name).clicked() {
                self.open_workspace(&name);
                ui.close();
            }
        }
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.workspace_input).hint_text("name").desired_width(120.0));
            let name = self.workspace_input.trim().to_string();
            if ui
                .add_enabled(!name.is_empty() && !self.open_projects.is_empty(), egui::Button::new("Save workspace"))
                .clicked()
            {
                self.save_workspace(&name);
                self.workspace_input.clear();
                ui.close();
            }
        });
    }

    fn settings_menu(&mut self, ui: &mut egui::Ui) {
        if ui.button("Edit config").clicked() {
            let _ = open_path(&self.config_path);
            ui.close();
        }
        if global_hooks_installed() {
            ui.label(RichText::new("Hooks for all Claude sessions are installed").size(12.0));
        } else if ui
            .button("Install hooks for all Claude sessions")
            .on_hover_text(
                "Adds `imacs hook` to ~/.claude/settings.json so agents in any terminal show up here and can be steered",
            )
            .clicked()
        {
            match install_global_hooks() {
                Ok(path) => self.toast(format!("Hooks added to {}", paths::display(&path))),
                Err(err) => self.toast(format!("Installing hooks failed: {err:#}")),
            }
            ui.close();
        }
        ui.separator();
        let keys = &self.config.keys;
        for (key, what) in [
            (&keys.quick_open, "open file"),
            (&keys.toggle_diff, "show/hide diff"),
            (&keys.next_attention, "next agent that needs you"),
            (&keys.focus_steer, "steer bar"),
            (&keys.new_session, "new session"),
            (&keys.voice, "voice: record, send (Esc cancels)"),
        ] {
            ui.label(RichText::new(format!("{key}  {what}")).size(12.0).monospace());
        }
    }

    fn pending_open_resume(&mut self, record: SessionRecord) {
        self.resume.push(record);
    }

    fn tab_strip(&mut self, ui: &mut egui::Ui, rect: Rect, tiled: &[usize], visible: &[usize]) {
        let p = self.palette.clone();
        ui.painter().rect_filled(rect, 0.0, p.panel);
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("tab-strip")
                .max_rect(rect.shrink2(vec2(6.0, 3.0)))
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        let mut clicked = None;
        egui::ScrollArea::horizontal().id_salt("tab-strip-scroll").show(&mut child, |ui| {
            ui.horizontal(|ui| {
                for &i in visible {
                    let s = &self.sessions[i];
                    let active = self.active.as_deref() == Some(s.id.as_str());
                    let shown = tiled.contains(&i);
                    let color = if active {
                        p.foreground
                    } else if shown {
                        lerp(p.muted, p.foreground, 0.5)
                    } else {
                        p.muted
                    };
                    let text =
                        RichText::new(format!("{}  {}", status_glyph(s.status), s.title)).color(color).size(12.5);
                    let response = ui.add(
                        egui::Button::new(text)
                            .fill(if active { lerp(p.panel, p.accent, 0.25) } else { Color32::TRANSPARENT })
                            .stroke(Stroke::NONE),
                    );
                    if response.clicked() {
                        clicked = Some(s.id.clone());
                    }
                }
            });
        });
        if let Some(id) = clicked {
            self.activate(&id, false);
        }
    }

    fn tiles(&mut self, ui: &mut egui::Ui, rect: Rect, layout: &Layout, tiled: &[usize]) {
        if layout.tiled == 0 || tiled.is_empty() {
            return;
        }
        let metrics = self.metrics(ui);
        let p = self.palette.clone();
        let (cols, rows) = (layout.columns.max(1), layout.rows.max(1));
        let size = vec2(rect.width() / cols as f32, rect.height() / rows as f32);
        let mut activate: Option<(String, bool)> = None;
        let mut close: Option<String> = None;
        let mut resume: Option<usize> = None;
        let focus_request = self.focus_terminal.take();
        let now = ui.input(|i| i.time);

        for (slot, &index) in tiled.iter().enumerate() {
            let min = rect.min + vec2((slot % cols) as f32 * size.x, (slot / cols) as f32 * size.y);
            let tile = Rect::from_min_size(min, size).shrink(3.0);
            let active = self.active.as_deref() == Some(self.sessions[index].id.as_str());
            let session = &mut self.sessions[index];

            let header = tile.with_max_y(tile.top() + TILE_HEADER);
            let mut body = tile.with_min_y(header.bottom());
            ui.painter().rect_filled(header, 2.0, if active { lerp(p.raised, p.accent, 0.12) } else { p.raised });
            ui.painter().line_segment([header.left_bottom(), header.right_bottom()], Stroke::new(1.0, p.border));
            if active {
                ui.painter().rect_filled(header.with_max_x(header.left() + 2.0), 0.0, p.accent);
            }
            ui.painter().circle_filled(
                pos2(header.left() + 12.0, header.center().y),
                3.5,
                status_dot(session.status, &p, now),
            );
            let title_rect = Rect::from_min_max(
                pos2(header.left() + 22.0, header.top()),
                pos2(header.right() - 28.0, header.bottom()),
            );
            let title_response =
                ui.interact(title_rect, ui.scope_id().with(("tile-title", &session.id)), Sense::click());
            let title_color = if title_response.hovered() || active { p.accent } else { p.foreground };
            let subtitle = match session.branch() {
                Some(branch) => format!("{} · {} · {}", session.agent, branch, session.status.label()),
                None => format!("{} · {}", session.agent, session.status.label()),
            };
            let galley = ui.fonts_mut(|f| f.layout_no_wrap(session.title.clone(), p.ui(13.0), title_color));
            let title_width = galley.size().x;
            ui.painter().with_clip_rect(title_rect).galley(
                pos2(title_rect.left(), header.center().y - galley.size().y / 2.0),
                galley,
                title_color,
            );
            ui.painter().with_clip_rect(title_rect).text(
                pos2(title_rect.left() + title_width + 10.0, header.center().y),
                egui::Align2::LEFT_CENTER,
                subtitle,
                p.ui(11.5),
                p.muted,
            );
            if title_response.on_hover_text("Show this worktree's diff against main").clicked() {
                activate = Some((session.id.clone(), true));
            }
            let close_rect = Rect::from_center_size(pos2(header.right() - 14.0, header.center().y), Vec2::splat(18.0));
            let close_response =
                ui.interact(close_rect, ui.scope_id().with(("tile-close", &session.id)), Sense::click());
            ui.painter().text(
                close_rect.center(),
                egui::Align2::CENTER_CENTER,
                "×",
                p.ui(15.0),
                if close_response.hovered() { p.error } else { p.muted },
            );
            if close_response.clicked() {
                close = Some(session.id.clone());
            }
            let edge = if active { p.accent.gamma_multiply(0.7) } else { p.border };
            ui.painter().rect_stroke(tile, 2.0, Stroke::new(1.0, edge), egui::StrokeKind::Outside);

            if let Some(pending) = session.approvals.first() {
                let request = &pending.request;
                let banner = body.with_max_y((body.top() + 76.0).min(body.bottom()));
                ui.painter().rect_filled(banner, 0.0, p.panel);
                let mut child = ui
                    .new_child(egui::UiBuilder::new().id_salt(("approval", &session.id)).max_rect(banner.shrink(6.0)));
                let details = format!(
                    "{}\n{}\n{}",
                    request.command.as_deref().unwrap_or(&request.tool),
                    request.cwd.display(),
                    request.reason.as_deref().unwrap_or("Permission requested")
                );
                child.label(RichText::new(&details).color(p.warning).size(11.5)).on_hover_text(details);
                body = body.with_min_y(banner.bottom());
            }
            let term_id = ui.scope_id().with(("term", &session.id));
            match &mut session.terminal {
                Some(terminal) => {
                    let response = terminal.show(ui, body, term_id, &metrics, &p);
                    if focus_request.as_deref() == Some(session.id.as_str()) {
                        response.request_focus();
                    }
                    if (response.clicked() || response.gained_focus()) && !active {
                        activate = Some((session.id.clone(), false));
                    }
                    if terminal.exited() {
                        let overlay = Rect::from_center_size(body.center(), vec2(body.width().min(320.0), 80.0));
                        ui.painter().rect_filled(overlay, 8.0, p.panel);
                        let mut child = ui.new_child(
                            egui::UiBuilder::new().id_salt(("exited", &session.id)).max_rect(overlay.shrink(12.0)),
                        );
                        child.vertical_centered(|ui| {
                            ui.label(RichText::new("Process exited").color(p.foreground));
                            ui.horizontal(|ui| {
                                if session.kind == AgentKind::Claude
                                    && session.claude_id.is_some()
                                    && ui.button("Resume").clicked()
                                {
                                    resume = Some(index);
                                }
                                if ui.button("Close").clicked() {
                                    close = Some(session.id.clone());
                                }
                            });
                        });
                    }
                }
                None => {
                    ui.painter().rect_filled(body, 0.0, p.background);
                    let mut child = ui.new_child(
                        egui::UiBuilder::new().id_salt(("outside", &session.id)).max_rect(body.shrink(14.0)),
                    );
                    child.label(
                        RichText::new(format!("Running outside IMACS in {}", paths::display(&session.cwd)))
                            .color(p.muted)
                            .size(12.0),
                    );
                    child.label(
                        RichText::new("Messages reach it at its next tool call or turn end.").color(p.muted).size(12.0),
                    );
                    child.add_space(8.0);
                    if let Some(prompt) = &session.last_prompt {
                        child.label(RichText::new("You").color(p.accent).size(12.0));
                        child.label(RichText::new(truncate(prompt, 600)).color(p.foreground));
                        child.add_space(6.0);
                    }
                    if let Some(message) = &session.last_message {
                        child.label(RichText::new("Claude").color(p.success).size(12.0));
                        child.label(RichText::new(truncate(message, 1500)).color(p.foreground));
                    }
                    if ui.interact(body, term_id, Sense::click()).clicked() && !active {
                        activate = Some((session.id.clone(), false));
                    }
                }
            }
        }

        if let Some((id, open)) = activate {
            self.activate(&id, open);
        }
        if let Some(id) = close {
            let running = self
                .sessions
                .iter()
                .find(|s| s.id == id)
                .and_then(|s| s.terminal.as_ref())
                .is_some_and(|t| !t.exited());
            if running { self.confirm_close = Some(id) } else { self.close(&id) }
        }
        if let Some(index) = resume {
            let s = &self.sessions[index];
            let record = SessionRecord {
                claude_session_id: s.claude_id.clone().unwrap_or_default(),
                agent: s.agent.clone(),
                cwd: s.cwd.clone(),
                title: s.title.clone(),
                last_prompt: None,
                updated: 0,
            };
            let id = s.id.clone();
            self.close(&id);
            self.resume.push(record);
        }
    }

    fn diff_panel(&mut self, ui: &mut egui::Ui, rect: Rect) {
        if self.panel == PanelMode::Plan {
            self.plan_panel(ui, rect);
            return;
        }
        let Some(root) = self.diff_target() else { return };
        // What changed sits right next to the diff/editor, in both modes.
        let rect = if rect.width() >= 760.0 {
            let column = rect.with_max_x(rect.left() + 250.0);
            let current = match self.panel {
                PanelMode::Editor => self.editor_file.get(&root).cloned(),
                _ => self.diffs.get(&root).and_then(|d| d.focused.clone()),
            };
            let p = self.palette.clone();
            let state = self.diffs.entry(root.clone()).or_default();
            if let Some((path, line)) = diff_view::changed_files(ui, column, state, current.as_deref(), &p) {
                match self.panel {
                    PanelMode::Editor => {
                        let ctx = ui.ctx().clone();
                        self.edit(&ctx, &root, Some(&path), line);
                    }
                    _ => {
                        let _ = self.diffs.entry(root.clone()).or_default().open(&path);
                    }
                }
            }
            rect.with_min_x(column.right())
        } else {
            rect
        };
        if self.panel == PanelMode::Editor {
            self.editor_panel(ui, rect, &root);
            return;
        }
        let session = self.active_index().map(|i| &self.sessions[i]);
        let title = session
            .map(|s| s.title.clone())
            .unwrap_or_else(|| root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        let branch = session
            .and_then(|s| s.branch().map(str::to_string))
            .or_else(|| self.worktrees.iter().find(|w| w.path == root).and_then(|w| w.branch.clone()))
            .filter(|b| *b != title);
        let can_send = self.sessions.iter().any(|s| s.root() == root && s.is_agent() && s.can_receive());
        let font = FontId::monospace(self.config.font.diff_size);
        let palette = self.palette.clone();
        let split = self.diff_split.unwrap_or(self.config.layout.diff_split);
        let state = self.diffs.entry(root.clone()).or_default();
        let header = Header { title: &title, branch: branch.as_deref(), can_send, split };
        match diff_view::show(ui, rect, state, header, &palette, &font) {
            DiffAction::None => {}
            DiffAction::SendReview => {
                self.send_review(&root);
            }
            DiffAction::Refresh => state_refresh(self.diffs.entry(root).or_default()),
            DiffAction::Close => self.diff_open = false,
            DiffAction::Split(on) => self.diff_split = Some(if on { DiffSplit::Always } else { DiffSplit::Never }),
            DiffAction::Edit { path, line } => {
                let ctx = ui.ctx().clone();
                self.edit(&ctx, &root, path.as_deref(), line);
            }
        }
    }

    /// Switches the panel to the editor for `root`, opening `path` at `line`. One
    /// editor runs per worktree and keeps running when you switch back to the diff.
    fn edit(&mut self, ctx: &egui::Context, root: &Path, path: Option<&str>, line: Option<u32>) {
        self.panel = PanelMode::Editor;
        self.diff_open = true;
        self.focus_editor = true;
        if let Some(path) = path {
            self.editor_file.insert(root.to_path_buf(), path.to_string());
        }
        let fill =
            |step: &String, path: &str| step.replace("{path}", path).replace("{line}", &line.unwrap_or(1).to_string());
        let open_steps = |path: &str| -> Vec<String> {
            self.config
                .editor
                .open_keys
                .iter()
                .filter(|step| line.is_some() || !step.contains("{line}"))
                .map(|step| fill(step, path))
                .collect()
        };
        if let Some(editor) = self.editors.get(root).filter(|e| !e.exited()) {
            if let Some(path) = path {
                editor.type_steps(open_steps(path), Duration::ZERO);
            }
            return;
        }
        let Some((program, rest)) = self.config.editor.command.split_first() else {
            self.toast("No editor configured ([editor] command)");
            return;
        };
        let mut args = rest.to_vec();
        args.push(path.unwrap_or(".").to_string());
        let mut env: HashMap<String, String> =
            [("TERM", "xterm-256color"), ("COLORTERM", "truecolor"), ("TERM_PROGRAM", "imacs")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        if let Some(shell_path) = SHELL_PATH.as_ref() {
            env.insert("PATH".into(), shell_path.clone());
        }
        let scrollback = self.config.terminal.scrollback;
        match Terminal::spawn(ctx, self.palette.clone(), resolve_program(program), args, root, env, scrollback) {
            Ok(editor) => {
                // The file is already open; only the theme and the jump to the line are left.
                let mut steps = self.config.editor.startup_keys.clone();
                if let Some(line) = line {
                    steps.extend(
                        self.config
                            .editor
                            .open_keys
                            .iter()
                            .filter(|step| step.contains("{line}"))
                            .map(|step| step.replace("{line}", &line.to_string())),
                    );
                }
                editor.type_steps(steps, Duration::from_millis(900));
                self.editors.insert(root.to_path_buf(), editor);
            }
            Err(err) => self.toast(format!("Could not start the editor: {err:#}")),
        }
    }

    fn editor_panel(&mut self, ui: &mut egui::Ui, rect: Rect, root: &Path) {
        let p = self.palette.clone();
        ui.painter().rect_filled(rect, 0.0, p.background);
        let header = rect.with_max_y(rect.top() + 30.0);
        let body = rect.with_min_y(header.bottom());
        ui.painter().rect_filled(header, 0.0, p.raised);
        ui.painter().line_segment([header.left_bottom(), header.right_bottom()], Stroke::new(1.0, p.border));
        let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("editor-header")
                .max_rect(header.shrink2(vec2(10.0, 3.0)))
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        child.label(RichText::new("EDITOR").color(p.accent).strong().size(12.0));
        child.label(RichText::new(&name).color(p.muted).size(12.0));
        let editor_running = self.editors.get(root).is_some_and(|e| !e.exited());
        let mut action = None;
        child.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("×").on_hover_text("Close the panel").clicked() {
                action = Some("close");
            }
            if ui
                .small_button("diff")
                .on_hover_text(format!("Back to the diff ({})", self.config.keys.toggle_editor))
                .clicked()
            {
                action = Some("diff");
            }
            if editor_running
                && !self.config.editor.inlay_hints_keys.is_empty()
                && ui.small_button("inlay hints").on_hover_text("Toggle the language server's inlay hints").clicked()
            {
                action = Some("hints");
            }
        });
        match action {
            Some("close") => self.diff_open = false,
            Some("diff") => self.panel = PanelMode::Diff,
            Some("hints") => {
                if let Some(editor) = self.editors.get(root) {
                    editor.type_steps(self.config.editor.inlay_hints_keys.clone(), Duration::ZERO);
                }
            }
            _ => {}
        }

        let metrics = self.metrics(ui);
        let id = ui.scope_id().with(("editor", root));
        self.editor_id = Some(id);
        match self.editors.get_mut(root).filter(|e| !e.exited()) {
            Some(editor) => {
                let response = editor.show(ui, body, id, &metrics, &p);
                if std::mem::take(&mut self.focus_editor) {
                    response.request_focus();
                }
            }
            None => {
                self.editors.remove(root);
                let mut child =
                    ui.new_child(egui::UiBuilder::new().id_salt("editor-empty").max_rect(body.shrink(20.0)));
                child.label(RichText::new("The editor is not running.").color(p.muted));
                if child.button("Open editor").clicked() {
                    let ctx = ui.ctx().clone();
                    self.edit(&ctx, root, None, None);
                }
            }
        }
    }

    fn welcome(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let p = self.palette.clone();
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
                for agent in self.config.agents.clone() {
                    if ui.button(format!("Start {}", agent.name)).clicked() {
                        let dir = self.launch_dir.clone();
                        self.pending_open.push((agent.name.clone(), dir));
                    }
                }
            });
            ui.add_space(6.0);
            ui.label(RichText::new(format!("in {}", paths::display(&self.launch_dir))).color(p.muted).size(12.0));
            ui.add_space(14.0);
            ui.label(RichText::new("Click an agent's title to see its worktree's diff against main.\nComment on any line and send the review straight to the agent.").color(p.muted).size(12.0));
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        self.quick_open_dialog(ctx);
        self.cleanup_dialog(ctx);
        if let Some(id) = self.confirm_close.clone() {
            let title = self.sessions.iter().find(|s| s.id == id).map(|s| s.title.clone());
            let Some(title) = title else {
                self.confirm_close = None;
                return;
            };
            let mut open = true;
            egui::Window::new("Close session?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label(format!("Closing “{title}” stops its process. Claude sessions can be resumed later."));
                    ui.horizontal(|ui| {
                        if ui.button("Close session").clicked() {
                            self.close(&id);
                            self.confirm_close = None;
                        }
                        if ui.button("Cancel").clicked() {
                            self.confirm_close = None;
                        }
                    });
                });
            if !open {
                self.confirm_close = None;
            }
        }

        let Some(mut dialog) = self.new_dialog.take() else { return };
        let mut keep = true;
        let mut start: Option<(String, PathBuf)> = None;
        let mut create: Option<(PathBuf, String, Launch)> = None;
        egui::Window::new("New session")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                egui::Grid::new("new-session").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                    ui.label("Agent");
                    egui::ComboBox::from_id_salt("agent").selected_text(&dialog.agent).show_ui(ui, |ui| {
                        for agent in &self.config.agents {
                            ui.selectable_value(&mut dialog.agent, agent.name.clone(), &agent.name);
                        }
                    });
                    ui.end_row();
                    ui.label("Where");
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut dialog.new_worktree, false, "Existing folder");
                        ui.radio_value(&mut dialog.new_worktree, true, "New worktree");
                    });
                    ui.end_row();
                    if dialog.new_worktree {
                        ui.label("Repository");
                        ui.text_edit_singleline(&mut dialog.repo);
                        ui.end_row();
                        ui.label("Branch");
                        ui.text_edit_singleline(&mut dialog.branch).request_focus();
                        ui.end_row();
                        if !dialog.branch.trim().is_empty() {
                            let path = git::worktree_path(
                                &self.config.worktree_path,
                                &paths::expand(dialog.repo.trim()),
                                dialog.branch.trim(),
                            );
                            ui.label("");
                            ui.label(RichText::new(paths::display(&path)).monospace().size(11.5));
                            ui.end_row();
                        }
                    } else {
                        ui.label("Folder");
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(&mut dialog.folder);
                            ui.menu_button("▾", |ui| {
                                let mut known: Vec<PathBuf> = self.worktrees.iter().map(|w| w.path.clone()).collect();
                                known.extend(self.sessions.iter().map(Session::root));
                                known.sort();
                                known.dedup();
                                for path in known {
                                    if ui.button(paths::display(&path)).clicked() {
                                        dialog.folder = paths::display(&path);
                                        ui.close();
                                    }
                                }
                            });
                        });
                        ui.end_row();
                    }
                });
                if let Some(err) = &dialog.error {
                    ui.label(RichText::new(err).color(self.palette.error));
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let enter = ui.input(|i| i.key_pressed(Key::Enter));
                    if ui.button("Start").clicked() || enter {
                        if dialog.new_worktree {
                            let repo = paths::expand(dialog.repo.trim());
                            let branch = dialog.branch.trim().to_string();
                            let launch = Launch {
                                agent: dialog.agent.clone(),
                                prompt: None,
                                args: Vec::new(),
                                reply: None,
                                task: None,
                            };
                            create = Some((repo, branch, launch));
                            keep = false;
                        } else {
                            start = Some((dialog.agent.clone(), paths::expand(dialog.folder.trim())));
                        }
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        keep = false;
                    }
                });
            });
        if let Some((repo, branch, launch)) = create {
            self.create_worktree(repo, branch, None, launch);
        }
        if let Some(target) = start {
            self.pending_open.push(target);
            keep = false;
        }
        if keep {
            self.new_dialog = Some(dialog);
        }
    }

    /// Fuzzy "go to file" over the worktree on screen.
    fn quick_open_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.quick_open.take() else { return };
        let Some(root) = self.diff_target() else { return };
        let Some(prepared) = self.diffs.get(&root).and_then(|s| s.prepared.clone()) else {
            self.quick_open = Some(dialog);
            return;
        };
        let mut all = prepared.all_files.to_vec();
        all.extend(prepared.diff.files.iter().map(|f| Arc::from(f.path())));
        all.sort();
        all.dedup();
        let changed: HashSet<&str> = prepared.diff.files.iter().map(|f| f.path()).collect();
        let p = self.palette.clone();
        let mut keep = true;
        let mut open: Option<String> = None;
        egui::Window::new("Go to file")
            .title_bar(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, vec2(0.0, 70.0))
            .fixed_size(vec2(620.0, 0.0))
            .show(ctx, |ui| {
                let matches = imacs_core::files::best_matches(&dialog.query, &all, 14);
                let (down, up, enter, escape) = ui.input_mut(|i| {
                    (
                        i.consume_key(egui::Modifiers::NONE, Key::ArrowDown),
                        i.consume_key(egui::Modifiers::NONE, Key::ArrowUp),
                        i.key_pressed(Key::Enter),
                        i.key_pressed(Key::Escape),
                    )
                });
                if down {
                    dialog.selected = (dialog.selected + 1).min(matches.len().saturating_sub(1));
                }
                if up {
                    dialog.selected = dialog.selected.saturating_sub(1);
                }
                let before = dialog.query.clone();
                let response = ui.add(
                    egui::TextEdit::singleline(&mut dialog.query)
                        .hint_text("Go to file · ↑↓ to pick · Enter opens · Esc closes")
                        .desired_width(f32::INFINITY)
                        .font(p.ui(15.0)),
                );
                if dialog.query != before {
                    dialog.selected = 0;
                }
                if std::mem::take(&mut dialog.focus) {
                    response.request_focus();
                }
                ui.add_space(4.0);
                for (i, path) in matches.iter().enumerate() {
                    let (dir, name) = match path.rfind('/') {
                        Some(cut) => (&path[..=cut], &path[cut + 1..]),
                        None => ("", *path),
                    };
                    let name_color = if changed.contains(path) { p.warning } else { p.foreground };
                    let mut job = egui::text::LayoutJob::default();
                    job.append(
                        name,
                        0.0,
                        egui::TextFormat { font_id: p.ui(14.0), color: name_color, ..Default::default() },
                    );
                    job.append(
                        &format!("   {dir}"),
                        0.0,
                        egui::TextFormat { font_id: p.ui(12.0), color: p.muted, ..Default::default() },
                    );
                    if ui.selectable_label(i == dialog.selected, job).clicked() {
                        open = Some(path.to_string());
                    }
                }
                if matches.is_empty() {
                    ui.label(RichText::new("No matching files").color(p.muted));
                }
                if enter && let Some(path) = matches.get(dialog.selected) {
                    open = Some(path.to_string());
                }
                if escape {
                    keep = false;
                }
            });
        if let Some(path) = open {
            self.open_file(&root, &path);
            keep = false;
        }
        if keep {
            self.quick_open = Some(dialog);
        }
    }

    fn screenshot(&mut self, ctx: &egui::Context) {
        let Some(shot) = &mut self.screenshot else { return };
        let now = ctx.input(|i| i.time);
        if shot.done.load(Ordering::Relaxed) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if !shot.requested && now >= shot.at {
            shot.requested = true;
            // A window hidden behind others is not painted, and an unpainted window cannot be captured.
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaintWhileHidden);
            let (path, done, repaint) = (shot.path.clone(), shot.done.clone(), ctx.clone());
            ctx.request_screenshot(move |image| {
                if let Err(err) = save_png(&path, &image) {
                    eprintln!("IMACS: screenshot failed: {err:#}");
                }
                done.store(true, Ordering::Relaxed);
                repaint.request_repaint();
            });
        }
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

impl eframe::App for App {
    /// Runs every frame, also while the window is hidden, so agents and remote
    /// control never wait on the window being visible.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.screenshot(ctx);
        for record in std::mem::take(&mut self.resume) {
            self.spawn(ctx, &record.agent, record.cwd.clone(), Some(record.claude_session_id));
        }
        self.pump(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.shortcuts(&ctx);
        let p = self.palette.clone();

        egui::Panel::top("top")
            .exact_size(34.0)
            .frame(egui::Frame::new().fill(p.panel).inner_margin(egui::Margin::symmetric(10, 4)))
            .show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("steer")
            .frame(egui::Frame::new().fill(p.panel).inner_margin(egui::Margin::symmetric(10, 7)))
            .show(ui, |ui| self.steer_bar(ui));

        let area = ui.available_rect_before_wrap();
        let diff_open = self.diff_open && (self.diff_target().is_some() || self.panel == PanelMode::Plan);
        let layout_config = layout::LayoutConfig { diff_width: self.split, ..self.config.layout.clone() };
        let visible = self.visible_sessions();
        let dock_width = if self.chief_open && self.chief_dock() == ChiefDock::Docked {
            self.store.chief_width.filter(|w| w.is_finite()).unwrap_or_else(|| {
                let sidebar =
                    if area.width() >= layout_config.sidebar_min_window { layout_config.sidebar_width } else { 0.0 };
                ((area.width() - sidebar) * 0.34).max(layout::MIN_DOCK_WIDTH)
            })
        } else {
            0.0
        };
        let mut layout =
            layout::compute(area.width(), area.height(), visible.len(), diff_open, dock_width, &layout_config);
        if layout.tabs {
            layout = layout::compute(
                area.width(),
                area.height() - TAB_HEIGHT,
                visible.len(),
                diff_open,
                dock_width,
                &layout_config,
            );
            layout.tabs = visible.len() > layout.tiled || layout.tiled == 0;
        }

        self.last_layout = Some(layout);
        if layout.sidebar {
            egui::Panel::left("sidebar")
                .exact_size(self.config.layout.sidebar_width)
                .resizable(false)
                .frame(egui::Frame::new().fill(p.panel).inner_margin(egui::Margin::symmetric(10, 4)))
                .show(ui, |ui| self.sidebar(ui));
        }

        let chief_rect = if layout.dock_width > 0.0 {
            Some(
                egui::Panel::left("chief-dock")
                    .exact_size(layout.dock_width)
                    .resizable(false)
                    .frame(egui::Frame::new().fill(p.background).inner_margin(egui::Margin::same(6)))
                    .show(ui, |ui| self.chief_content(ui))
                    .response
                    .rect,
            )
        } else {
            None
        };

        egui::CentralPanel::default().frame(egui::Frame::new().fill(p.background)).show(ui, |ui| {
            let mut rect = ui.max_rect();
            let statuses: Vec<AgentStatus> = visible.iter().map(|&i| self.sessions[i].status).collect();
            let active = self.active_index().and_then(|a| visible.iter().position(|&i| i == a));
            let tiled: Vec<usize> =
                pick_tiled(&statuses, active, layout.tiled).into_iter().map(|k| visible[k]).collect();
            if layout.tabs {
                self.tab_strip(ui, rect.with_max_y(rect.top() + TAB_HEIGHT), &tiled, &visible);
                rect.min.y += TAB_HEIGHT;
            }
            if visible.is_empty() && !diff_open {
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
                if visible.is_empty() {
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
                        self.split = Some(width - response.drag_delta().x);
                    }
                    if response.double_clicked() {
                        self.split = None;
                    }
                }
                let edge = match layout.diff {
                    DiffPlacement::Right { .. } => Some([diff_rect.left_top(), diff_rect.left_bottom()]),
                    DiffPlacement::Top { .. } => Some([diff_rect.left_bottom(), diff_rect.right_bottom()]),
                    _ => None,
                };
                if let Some(edge) = edge {
                    ui.painter().line_segment(edge, Stroke::new(1.0, lerp(p.panel, p.muted, 0.35)));
                }
            }
        });

        if let Some(rect) = chief_rect
            && chief_divider(ui, rect, &mut self.store.chief_width, &p)
        {
            self.save_chief_preferences();
        }

        self.chief_ui(&ctx);
        self.dialogs(&ctx);
    }

    fn on_exit(&mut self) {
        for editor in self.editors.values() {
            editor.shutdown();
        }
        for session in &self.sessions {
            if let Some(terminal) = &session.terminal {
                terminal.shutdown();
            }
        }
        let _ = std::fs::remove_file(paths::socket_path());
    }
}

fn chief_divider(ui: &mut egui::Ui, rect: Rect, width: &mut Option<f32>, p: &Palette) -> bool {
    let handle = Rect::from_center_size(rect.right_center(), vec2(8.0, rect.height()));
    let response = ui.interact(handle, egui::Id::NULL.with("chief-divider"), Sense::click_and_drag());
    ui.painter().line_segment([rect.right_top(), rect.right_bottom()], Stroke::new(1.0, p.border));
    if response.hovered() || response.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        ui.painter().rect_filled(handle.shrink2(vec2(3.0, 0.0)), 0.0, p.accent.gamma_multiply(0.6));
    }
    if (response.dragged() || response.drag_stopped())
        && let Some(pointer) = response.interact_pointer_pos()
    {
        *width = Some((pointer.x - rect.left()).max(layout::MIN_DOCK_WIDTH));
    }
    if response.double_clicked() {
        *width = None;
    }
    response.drag_stopped() || response.double_clicked()
}

fn state_refresh(state: &mut DiffState) {
    state.dirty_since = Some(Instant::now() - Duration::from_secs(1));
}

fn section(ui: &mut egui::Ui, p: &Palette, title: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("▍").color(p.accent).size(11.0));
        ui.label(RichText::new(title).size(10.5).color(p.muted).strong());
    });
    ui.add_space(1.0);
}

fn session_row(
    ui: &mut egui::Ui,
    p: &Palette,
    s: &Session,
    active: bool,
    stats: Option<(usize, usize)>,
) -> egui::Response {
    let height = 38.0;
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
    let painter = ui.painter();
    if active {
        painter.rect_filled(rect, 2.0, lerp(p.panel, p.accent, 0.14));
        painter.rect_filled(rect.with_max_x(rect.left() + 2.0), 0.0, p.accent);
    } else if response.hovered() {
        painter.rect_filled(rect, 2.0, p.raised);
    }
    let now = ui.input(|i| i.time);
    painter.circle_filled(pos2(rect.left() + 10.0, rect.top() + 13.0), 3.5, status_dot(s.status, p, now));
    let clip = painter.with_clip_rect(rect.shrink2(vec2(2.0, 0.0)));
    clip.text(
        pos2(rect.left() + 20.0, rect.top() + 5.0),
        egui::Align2::LEFT_TOP,
        &s.title,
        p.ui(13.0),
        if active { p.accent } else { p.foreground },
    );
    let mut detail = format!("{} · {}", s.agent, s.status.label());
    if s.terminal.is_none() {
        detail.push_str(" · outside");
    }
    if s.selected {
        detail.push_str(" · ✔");
    }
    clip.text(pos2(rect.left() + 20.0, rect.top() + 21.0), egui::Align2::LEFT_TOP, detail, p.ui(11.0), p.muted);
    if let Some((add, del)) = stats {
        let text = format!("+{add} −{del}");
        clip.text(
            pos2(rect.right() - 6.0, rect.top() + 5.0),
            egui::Align2::RIGHT_TOP,
            text,
            FontId::monospace(10.5),
            p.muted,
        );
    }
    response
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

fn task_color(status: TaskStatus, p: &Palette) -> Color32 {
    match status {
        TaskStatus::Planned => p.muted,
        TaskStatus::Running => p.accent,
        TaskStatus::Blocked => p.warning,
        TaskStatus::Review => p.info,
        TaskStatus::Done => p.success,
    }
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

fn open_path(path: &Path) -> std::io::Result<std::process::Child> {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    std::process::Command::new(opener).arg(path).spawn()
}

fn save_png(path: &Path, image: &egui::ColorImage) -> anyhow::Result<()> {
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), image.size[0] as u32, image.size[1] as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    let bytes: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
    writer.write_image_data(&bytes)?;
    Ok(())
}

fn apply_style(ctx: &egui::Context, config: &Config) {
    let p = Palette::from_config(&config.theme);
    let mut style = egui::Style::default();
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = p.panel;
    visuals.window_fill = p.raised;
    visuals.window_stroke = Stroke::new(1.0, p.border);
    visuals.window_corner_radius = egui::CornerRadius::same(4);
    visuals.menu_corner_radius = egui::CornerRadius::same(3);
    visuals.extreme_bg_color = p.background;
    visuals.faint_bg_color = p.raised;
    visuals.code_bg_color = p.raised;
    visuals.selection.bg_fill = p.accent.gamma_multiply(0.35);
    visuals.selection.stroke = Stroke::new(1.0, p.accent);
    visuals.hyperlink_color = p.accent;
    let widgets = &mut visuals.widgets;
    for state in [
        &mut widgets.noninteractive,
        &mut widgets.inactive,
        &mut widgets.hovered,
        &mut widgets.active,
        &mut widgets.open,
    ] {
        state.corner_radius = egui::CornerRadius::same(2);
        state.expansion = 0.0;
    }
    widgets.noninteractive.bg_fill = p.panel;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.foreground);
    widgets.inactive.bg_fill = p.raised;
    widgets.inactive.weak_bg_fill = p.raised;
    widgets.inactive.bg_stroke = Stroke::new(1.0, p.border);
    widgets.inactive.fg_stroke = Stroke::new(1.0, p.foreground);
    widgets.hovered.bg_fill = lerp(p.raised, p.accent, 0.2);
    widgets.hovered.weak_bg_fill = lerp(p.raised, p.accent, 0.2);
    widgets.hovered.bg_stroke = Stroke::new(1.0, p.accent.gamma_multiply(0.8));
    widgets.hovered.fg_stroke = Stroke::new(1.0, p.foreground);
    widgets.active.bg_fill = lerp(p.raised, p.accent, 0.35);
    widgets.active.weak_bg_fill = lerp(p.raised, p.accent, 0.35);
    widgets.active.bg_stroke = Stroke::new(1.0, p.accent);
    widgets.open.weak_bg_fill = lerp(p.raised, p.accent, 0.25);
    style.visuals = visuals;
    if p.mono_ui {
        for font in style.text_styles.values_mut() {
            font.family = egui::FontFamily::Monospace;
            font.size = (font.size * 0.92).round();
        }
    }
    style.spacing.button_padding = vec2(7.0, 2.0);
    style.spacing.item_spacing = vec2(7.0, 4.0);
    ctx.set_global_style(style);
}

/// `\r`, `\n`, `\t`, `\e` and `\xNN` escapes in raw key input.
fn unescape(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('e') => out.push('\x1b'),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                    out.push(byte as char);
                }
            }
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// The chief's mark: a planet with a tilted orbit and a moon, after the atomisk.ai icon.
fn orbit_icon(painter: &egui::Painter, rect: Rect, accent: Color32, light: Color32) {
    let c = rect.center();
    let r = rect.width() / 2.0;
    painter.circle_filled(c, r * 0.32, light);
    let tilt = -23f32.to_radians();
    let points: Vec<Pos2> = (0..=48)
        .map(|k| {
            let t = k as f32 / 48.0 * std::f32::consts::TAU;
            let (x, y) = (r * 0.95 * t.cos(), r * 0.38 * t.sin());
            pos2(c.x + x * tilt.cos() - y * tilt.sin(), c.y + x * tilt.sin() + y * tilt.cos())
        })
        .collect();
    painter.add(egui::Shape::line(points, Stroke::new(1.4, accent)));
    let moon = pos2(c.x + r * 0.95 * tilt.cos(), c.y + r * 0.95 * tilt.sin());
    painter.circle_filled(moon, r * 0.14, accent);
}

/// Longest first prompt passed on the command line to agents other than Claude.
const LONG_PROMPT: usize = 700;

/// Writes a long prompt to a brief file and returns the short prompt that points to it.
fn brief_file(session: &str, text: &str) -> Result<String, String> {
    let dir = paths::briefs_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let path = dir.join(format!("{session}.md"));
    std::fs::write(&path, text).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(format!("Your task brief is in {}. Read it first and follow it exactly.", path.display()))
}

/// Tools the chief may use without asking: reading, planning, reviewing, and running the
/// plan the user approved. Starting agents, typing into terminals and closing sessions ask first.
const CHIEF_ALLOWED_TOOLS: &[&str] = &[
    "mcp__imacs__imacs_list",
    "mcp__imacs__imacs_wait",
    "mcp__imacs__imacs_approve",
    "mcp__imacs__imacs_result",
    "mcp__imacs__imacs_read",
    "mcp__imacs__imacs_diff",
    "mcp__imacs__imacs_plan_get",
    "mcp__imacs__imacs_plan_close",
    "mcp__imacs__imacs_plan_set",
    "mcp__imacs__imacs_plan_update",
    "mcp__imacs__imacs_plan_dispatch",
    "mcp__imacs__imacs_plan_integrate",
    "mcp__imacs__imacs_comment",
    "mcp__imacs__imacs_send_review",
    "mcp__imacs__imacs_focus",
    "mcp__imacs__imacs_open",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, kind: AgentKind, status: AgentStatus, cwd: &Path) -> Session {
        Session {
            id: id.into(),
            agent: "test".into(),
            kind,
            terminal: None,
            claude_id: Some(id.into()),
            cwd: cwd.into(),
            worktree: None,
            title: id.into(),
            status,
            last_prompt: None,
            last_message: None,
            hooks_active: false,
            approvals: Vec::new(),
            allowed: Vec::new(),
            held: Vec::new(),
            approval_hold: false,
            menu_absent_since: None,
            selected: false,
            chief: false,
            pending: None,
        }
    }

    fn with_app(name: &str, test: impl FnOnce(&mut App, &egui::Context)) {
        if std::env::var_os("IMACS_APP_TEST_CHILD").is_none() {
            let dir = tempfile::tempdir().unwrap();
            let output = crate::test_support::output(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", name, "--nocapture"])
                    .current_dir(dir.path())
                    .env("IMACS_APP_TEST_CHILD", "1")
                    .env("CODEX_HOME", dir.path().join("codex"))
                    .env("HOME", dir.path())
                    .env("IMACS_HOME", dir.path().join("state"))
                    .env("IMACS_CONFIG", dir.path().join("config.toml"))
                    .env("IMACS_SOCKET", dir.path().join("imacs.sock"))
                    .env_remove("IMACS_PANE"),
            );
            assert!(
                output.status.success(),
                "stdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = App::new(
            &cc,
            Options { migration_errors: vec!["Migration failed".into()], ..Options::default() },
            &crate::load_icon(),
        );
        assert_eq!(app.toast.as_ref().unwrap().0, "Migration failed");
        test(&mut app, &ctx);
    }

    #[test]
    fn chief_approval_access_and_read_only_defaults_are_documented() {
        assert!(CHIEF_ALLOWED_TOOLS.contains(&"mcp__imacs__imacs_approve"));
        let readme = include_str!("../../../README.md");
        assert!(readme.contains("pre-approved `imacs_approve`"));
        assert!(readme.contains("`allow_always` for the rest"));
        assert!(!readme.contains("or answer other agents'"));
        assert!(readme.contains("outside Codex's sandbox"));
    }

    #[test]
    fn codex_version_is_cached_per_program_without_blocking_launch() {
        with_app("app::tests::codex_version_is_cached_per_program_without_blocking_launch", |app, ctx| {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let script = r#"#!/bin/sh
dir=${0%/*}
if [ "$1" = --version ]; then
  printf 'checked\n' >> "$dir/checks"
  attempts=0
  while [ ! -e "$dir/ready" ]; do
    attempts=$((attempts + 1))
    [ "$attempts" -lt 200 ] || exit 1
    sleep 0.01
  done
  printf 'codex-cli 0.165.0\n'
else
  printf '%s\n' "$@" > "$dir/launch-args"
fi
"#;
            for name in ["one", "two"] {
                let folder = dir.path().join(name);
                std::fs::create_dir(&folder).unwrap();
                let program = folder.join("codex");
                std::fs::write(
                    &program,
                    if name == "two" { script.replace("0.165.0", "0.164.0") } else { script.to_string() },
                )
                .unwrap();
                std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
                app.config.agents.push(imacs_core::config::AgentConfig {
                    name: name.into(),
                    command: vec![program.to_string_lossy().into_owned()],
                    kind: AgentKind::Generic,
                    busy_text: None,
                });
            }
            let probe_dir = dir.path().to_path_buf();
            let release = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while ["one", "two"].iter().any(|name| !probe_dir.join(name).join("checks").exists()) {
                    assert!(Instant::now() < deadline, "both asynchronous version probes must start");
                    std::thread::sleep(Duration::from_millis(10));
                }
                for name in ["one", "two"] {
                    std::fs::write(probe_dir.join(name).join("ready"), "").unwrap();
                }
            });
            let program = |name: &str| dir.path().join(name).join("codex").to_string_lossy().into_owned();
            let one = app.codex_version(ctx, program("one"));
            assert!(Arc::ptr_eq(&one, &app.codex_version(ctx, program("one"))));
            let two = app.codex_version(ctx, program("two"));
            release.join().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while one.get().is_none() || two.get().is_none() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(one.get(), Some(&true));
            assert_eq!(two.get(), Some(&false));
            assert_eq!(app.codex_versions.len(), 2);
            // A gated probe must finish before start() runs git: concurrent children can
            // inherit pipe writers on macOS and otherwise keep git's output() waiting for EOF.
            let mut ids = Vec::new();
            for name in ["one", "one", "two"] {
                ids.push(app.start(ctx, name, dir.path().into(), None, vec![], None).unwrap());
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while ids.iter().any(|id| !app.sessions[app.find(id).unwrap()].terminal.as_ref().unwrap().exited()) {
                assert!(Instant::now() < deadline, "finite fake agents must exit before teardown");
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                std::fs::read_to_string(dir.path().join("one/launch-args"))
                    .unwrap()
                    .contains("hooks.PermissionRequest")
            );
            assert!(
                !std::fs::read_to_string(dir.path().join("two/launch-args"))
                    .unwrap()
                    .contains("hooks.PermissionRequest")
            );
            assert!(Arc::ptr_eq(&one, &app.codex_version(ctx, program("one"))));
            for name in ["one", "two"] {
                assert_eq!(std::fs::read_to_string(dir.path().join(name).join("checks")).unwrap().lines().count(), 1);
            }
            app.sessions.clear();
        });
    }

    #[test]
    fn expired_approvals_wait_for_menu_quiet_or_lifecycle() {
        with_app("app::tests::expired_approvals_wait_for_menu_quiet_or_lifecycle", |app, ctx| {
            let dir = tempfile::tempdir().unwrap();
            app.sessions = vec![session("stale", AgentKind::Generic, AgentStatus::NeedsInput, dir.path())];
            let permission = imacs_core::hook::HookEvent::from_payload(&json!({
                "hook_event_name":"PermissionRequest", "session_id":"stale", "cwd":dir.path(),
                "tool_name":"Bash", "tool_input":{"command":"touch fixture", "description":"test"}
            }))
            .unwrap();
            let reply = Arc::new(ipc::ApprovalReply::default());
            app.sessions[0].approval_hold = true;
            app.sessions[0].approvals.push(PendingApproval {
                request: ApprovalRequest::from_event(&permission),
                reply: Some(reply.clone()),
            });
            let raw = request(app, json!({"cmd":"send", "session":"stale", "raw":true, "text":"y"}));
            assert!(!raw.ok);
            assert!(raw.error.unwrap().contains("imacs_approve"));
            assert_eq!(app.deliver(0, "held plain message"), Delivery::Held);
            let now = Instant::now();
            app.approvals_checked = now - APPROVAL_CHECK_INTERVAL;
            app.expire_approvals(now);
            assert_eq!(app.sessions[0].approvals.len(), 1);
            reply.wait(Duration::ZERO);
            let first_check = now + APPROVAL_CHECK_INTERVAL;
            app.expire_approvals(first_check);
            assert_eq!(app.sessions[0].approvals.len(), 1, "an expired hook may precede its menu");
            assert_eq!(app.sessions[0].held, ["held plain message"]);
            let second_check = first_check + APPROVAL_CHECK_INTERVAL;
            app.expire_approvals(second_check);
            assert_eq!(app.sessions[0].approvals.len(), 1, "the menu quiet period must elapse");
            app.sessions[0].menu_absent_since = Some(first_check - APPROVAL_MENU_GRACE);
            app.expire_approvals(second_check);
            assert_eq!(app.sessions[0].approvals.len(), 1, "screen checks must be throttled");
            app.expire_approvals(second_check + APPROVAL_CHECK_INTERVAL);
            assert!(app.sessions[0].approvals.is_empty());
            assert!(app.sessions[0].held.is_empty());
            assert_eq!(app.hub.lock().unwrap().outbox["stale"], ["held plain message"]);
            let menu = "Allow these file edits?\n› 1. Yes, proceed\n  2. No\nPress enter to confirm or esc to cancel";
            assert!(imacs_core::approvals::menu_key(menu, Some("touch fixture"), Decision::Allow).is_none());
            let terminal = Terminal::spawn(
                ctx,
                app.palette.clone(),
                "/bin/sh".into(),
                vec!["-c".into(), "printf '%s' \"$1\"; exec sleep 10".into(), "fixture".into(), menu.into()],
                dir.path(),
                HashMap::new(),
                100,
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while !terminal.screen_text(60).contains("esc to cancel") {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            app.sessions[0].terminal = Some(terminal);
            app.sessions[0].status = AgentStatus::NeedsInput;
            app.sessions[0].approval_hold = true;
            app.sessions[0].menu_absent_since = Some(Instant::now() - APPROVAL_MENU_GRACE);
            app.sessions[0]
                .approvals
                .push(PendingApproval { request: ApprovalRequest::from_event(&permission), reply: Some(reply) });
            assert_eq!(app.deliver(0, "held while editing"), Delivery::Held);
            let before = app.sessions[0].terminal.as_ref().unwrap().screen_text(60);
            app.approvals_checked = Instant::now() - APPROVAL_CHECK_INTERVAL;
            app.expire_approvals(Instant::now());
            assert_eq!(app.sessions[0].approvals.len(), 1);
            assert!(app.sessions[0].menu_absent_since.is_none(), "a menu drawn late resets the quiet period");
            assert_eq!(app.sessions[0].held, ["held while editing"]);
            let event = imacs_core::hook::HookEvent::from_payload(&json!({"hook_event_name":"PreToolUse", "session_id":"stale", "cwd":dir.path(), "tool_name":"Bash", "tool_input":{"command":"cat fixture"}})).unwrap();
            app.on_hook(HookMessage { pane: None, ancestors: vec![], event, delivered: 0, approval_reply: None });
            assert!(app.sessions[0].approvals.is_empty());
            assert_eq!(app.sessions[0].status, AgentStatus::Working);
            assert!(app.sessions[0].approval_hold, "PreToolUse alone must not release input into a menu");
            assert_eq!(app.deliver(0, "still held"), Delivery::Held);
            app.approvals_checked = Instant::now() - APPROVAL_CHECK_INTERVAL;
            app.expire_approvals(Instant::now());
            assert_eq!(app.sessions[0].held.len(), 2);
            assert_eq!(app.sessions[0].terminal.as_ref().unwrap().screen_text(60), before);
            app.sessions[0].terminal = None;
            let event = imacs_core::hook::HookEvent::from_payload(
                &json!({"hook_event_name":"Interrupt", "session_id":"stale", "cwd":dir.path()}),
            )
            .unwrap();
            app.on_hook(HookMessage { pane: None, ancestors: vec![], event, delivered: 0, approval_reply: None });
            assert!(!app.sessions[0].approval_hold);
            assert!(app.sessions[0].held.is_empty());
            assert!(
                app.hub.lock().unwrap().outbox["stale"].last().unwrap().contains("held while editing\n\nstill held")
            );
            app.sessions.clear();
        });
    }

    #[test]
    fn review_followups() {
        with_app("app::tests::review_followups", |app, ctx| {
            permission_hooks_wake_waiters_and_accept_decisions(app, ctx);
            busy_text_is_only_a_fallback(app, ctx);
            close_idle_leaves_shells_alone(app);
            voice_to_a_shell_stays_in_the_steer_bar(app, ctx);
            plan_set_resolves_cached_worktree_projects(app);
            dispatch_requires_integrated_dependencies(app);
        });
    }

    fn request(app: &mut App, value: serde_json::Value) -> ControlResponse {
        let (tx, rx) = channel();
        app.control(&app.ctx.clone(), serde_json::from_value(value).unwrap(), tx);
        match rx.recv_timeout(Duration::from_secs(2)).unwrap() {
            ControlOutcome::Done(response) => response,
            _ => panic!("unexpected deferred response"),
        }
    }

    fn repository() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git::git(&repo, &["init", "-q", "-b", "main"]).unwrap();
        git::git(&repo, &["config", "user.name", "Test"]).unwrap();
        git::git(&repo, &["config", "user.email", "test@example.org"]).unwrap();
        git::git(&repo, &["commit", "--allow-empty", "-qm", "initial"]).unwrap();
        (dir, repo)
    }

    #[test]
    fn plan_close_persists_status_and_restores_implicit_single_plan_selection() {
        with_app("app::tests::plan_close_persists_status_and_restores_implicit_single_plan_selection", |app, _| {
            let one =
                request(app, json!({"cmd":"plan_set", "goal":"One", "tasks":[{"id":"a"}]})).result["plan"].clone();
            let two =
                request(app, json!({"cmd":"plan_set", "goal":"Two", "tasks":[{"id":"a"}]})).result["plan"].clone();
            assert!(!request(app, json!({"cmd":"plan_close", "plan":two, "status":"open"})).ok);
            let closed = request(app, json!({"cmd":"plan_close", "plan":two, "status":"completed"}));
            assert!(closed.ok);
            assert_eq!(closed.result["status"], "completed");
            assert!(request(app, json!({"cmd":"plan_update", "id":"a", "status":"done"})).ok);
            assert_eq!(app.plans[app.selected_plan].id, one.as_str().unwrap());
            assert_eq!(request(app, json!({"cmd":"plan_get"})).result["plans"].as_array().unwrap().len(), 1);
            assert!(request(app, json!({"cmd":"plan_close", "plan":one, "status":"archived"})).ok);
            assert!(!app.plan_view);
            assert!(app.plans.iter().all(|p| !p.is_open()));
            let (loaded, errors) = plan::load_plans(&paths::imacs_home()).unwrap();
            assert!(errors.is_empty());
            assert_eq!(loaded.iter().find(|p| p.id == one.as_str().unwrap()).unwrap().status, PlanStatus::Archived);
            assert_eq!(loaded.iter().find(|p| p.id == two.as_str().unwrap()).unwrap().status, PlanStatus::Completed);
            assert!(CHIEF_ALLOWED_TOOLS.contains(&"mcp__imacs__imacs_plan_close"));
        });
    }

    #[test]
    fn startup_toast_lists_unreadable_plan_files_without_hiding_valid_plans() {
        with_app("app::tests::startup_toast_lists_unreadable_plan_files_without_hiding_valid_plans", |app, ctx| {
            request(app, json!({"cmd":"plan_set", "goal":"Valid", "tasks":[]}));
            let folder = paths::imacs_home().join("plans");
            std::fs::write(folder.join("broken.json"), "{").unwrap();
            std::fs::create_dir(folder.join("unreadable.json")).unwrap();
            let cc = eframe::CreationContext::_new_kittest(ctx.clone());
            let reopened = App::new(&cc, Options::default(), &crate::load_icon());
            let toast = &reopened.toast.as_ref().unwrap().0;
            assert!(toast.contains("broken.json") && toast.contains("unreadable.json"), "{toast}");
            assert_eq!(reopened.plans.iter().filter(|p| p.is_open()).count(), 1);
        });
    }

    #[test]
    fn background_plan_updates_and_successful_dispatch_preserve_the_foreground_and_chief() {
        with_app(
            "app::tests::background_plan_updates_and_successful_dispatch_preserve_the_foreground_and_chief",
            |app, ctx| {
                let (dir, repo) = repository();
                app.config.worktree_path = format!("{}/{{slug}}", dir.path().display());
                app.config.agents.push(imacs_core::config::AgentConfig {
                    name: "fixture".into(),
                    command: vec!["/bin/cat".into()],
                    kind: AgentKind::Shell,
                    busy_text: None,
                });
                let first = request(app, json!({"cmd":"plan_set", "goal":"Background", "tasks":[{"id":"a", "repo":repo, "agent":"fixture"}]})).result["plan"].clone();
                let one = app.selected_plan;
                request(app, json!({"cmd":"plan_set", "goal":"Foreground", "tasks":[{"id":"a"}]}));
                let two = app.selected_plan;
                app.sessions.push(session("foreground", AgentKind::Generic, AgentStatus::Idle, &repo));
                app.active = Some("foreground".into());
                app.diff_root = Some(repo.clone());
                app.focus_terminal = Some("foreground".into());
                app.chief_open = true;
                app.panel = PanelMode::Editor;
                assert!(
                    request(
                        app,
                        json!({"cmd":"plan_set", "plan":first, "goal":"Background revised", "tasks":[
                {"id":"a", "repo":repo, "agent":"fixture"}]})
                    )
                    .ok
                );
                assert_eq!(app.selected_plan, two);
                assert!(app.chief_open && app.panel == PanelMode::Editor);
                assert_eq!(app.active.as_deref(), Some("foreground"));
                assert_eq!(app.diff_root.as_ref(), Some(&repo));
                assert!(app.changed_plans.contains(first.as_str().unwrap()));
                assert!(app.toast.as_ref().unwrap().0.starts_with("Plan updated:"));
                app.plans[one].approved = true;
                assert!(request(app, json!({"cmd":"plan_dispatch", "plan":first, "id":"a"})).ok);
                let deadline = Instant::now() + Duration::from_secs(10);
                while !app.creating.is_empty() {
                    assert!(Instant::now() < deadline);
                    app.pump(ctx);
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(app.plans[one].tasks[0].session.is_some());
                assert_eq!(app.selected_plan, two);
                assert!(app.chief_open && app.panel == PanelMode::Editor);
                assert_eq!(app.active.as_deref(), Some("foreground"));
                assert_eq!(app.diff_root.as_ref(), Some(&repo));
                assert_eq!(app.focus_terminal.as_deref(), Some("foreground"));
                assert!(app.changed_plans.contains(first.as_str().unwrap()));
                app.selected_plan = one;
                app.show_plan_fleet();
                assert!(!app.changed_plans.contains(first.as_str().unwrap()));
                assert!(
                    request(
                        app,
                        json!({"cmd":"plan_set", "plan":first, "goal":"Selected update", "tasks":[{"id":"a"}]})
                    )
                    .ok
                );
                assert!(app.panel == PanelMode::Plan);
                assert!(!app.chief_open);
            },
        );
    }

    #[test]
    fn status_updates_clear_integration_and_dispatch_rejects_new_unmerged_commits() {
        with_app("app::tests::status_updates_clear_integration_and_dispatch_rejects_new_unmerged_commits", |app, _| {
            let (_dir, repo) = repository();
            git::git(&repo, &["branch", "task-a"]).unwrap();
            git::git(&repo, &["branch", "integration"]).unwrap();
            let plan = request(
                app,
                json!({"cmd":"plan_set", "goal":"Freshness", "integration":"integration", "tasks":[
                {"id":"a", "repo":repo, "branch":"task-a"}, {"id":"b", "repo":repo, "depends_on":["a"]}]}),
            )
            .result["plan"]
                .clone();
            let index = app.selected_plan;
            app.plans[index].approved = true;
            app.plans[index].tasks[0].status = TaskStatus::Done;
            app.plans[index].tasks[0].integrated = Some("integration".into());
            assert!(request(app, json!({"cmd":"plan_update", "plan":plan, "id":"a", "status":"running"})).ok);
            assert!(app.plans[index].tasks[0].integrated.is_none());
            app.finish_integration(Integrated {
                plan: plan.as_str().unwrap().into(),
                task: "a".into(),
                branch: "task-a".into(),
                into: "integration".into(),
                reply: None,
                result: Ok(Integration { committed: false, merge: Merge::UpToDate, worktree: repo.clone() }),
            });
            assert!(
                app.plans[index].tasks[0].integrated.is_none(),
                "a late integration must not mark a running task integrated"
            );
            assert!(request(app, json!({"cmd":"plan_update", "plan":plan, "id":"a", "status":"done"})).ok);
            assert!(app.dispatch(index, Some("b")).unwrap_err().contains("integrated dependencies"));
            app.plans[index].tasks[0].integrated = Some("integration".into());
            git::git(&repo, &["switch", "task-a"]).unwrap();
            git::git(&repo, &["commit", "--allow-empty", "-qm", "new work after integration"]).unwrap();
            assert!(app.dispatch(index, Some("b")).unwrap_err().contains("integrated dependencies"));
            assert!(app.plans[index].tasks[0].integrated.is_none());
            assert!(app.creating.is_empty());
            let saved = Plan::load(&paths::imacs_home().join("plans").join(format!("{}.json", plan.as_str().unwrap())))
                .unwrap();
            assert!(saved.tasks[0].integrated.is_none());
            git::git(&repo, &["branch", "-f", "integration", "task-a"]).unwrap();
            app.plans[index].tasks[0].integrated = Some("integration".into());
            // A missing requested task does not invalidate a genuinely integrated dependency.
            assert!(app.dispatch(index, Some("missing")).is_err());
            assert_eq!(app.plans[index].tasks[0].integrated.as_deref(), Some("integration"));
        });
    }

    #[test]
    fn integrations_in_different_repositories_can_finish_independently() {
        use std::os::unix::fs::PermissionsExt;
        with_app("app::tests::integrations_in_different_repositories_can_finish_independently", |app, _| {
            let (dir1, repo1) = repository();
            let (_dir2, repo2) = repository();
            let started = dir1.path().join("started");
            let release = dir1.path().join("release");
            let hook = repo1.join(".git/hooks/pre-commit");
            std::fs::write(
                &hook,
                format!(
                    "#!/bin/sh\ntouch '{}'\nwhile [ ! -f '{}' ]; do sleep 0.02; done\n",
                    started.display(),
                    release.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
            app.config.worktree_path = "{repo_parent}/{slug}".into();
            let mut indices = Vec::new();
            for (goal, repo) in [("One", &repo1), ("Two", &repo2)] {
                git::git(repo, &["branch", "task"]).unwrap();
                let worktree = repo.with_file_name("task-worktree");
                git::git(repo, &["worktree", "add", worktree.to_str().unwrap(), "task"]).unwrap();
                std::fs::write(worktree.join("work.txt"), goal).unwrap();
                request(
                    app,
                    json!({"cmd":"plan_set", "goal":goal, "tasks":[{"id":"a", "repo":repo, "branch":"task"}]}),
                );
                let index = app.selected_plan;
                app.plans[index].tasks[0].status = TaskStatus::Done;
                indices.push(index);
            }
            app.integrate(indices[0], "a", None);
            let deadline = Instant::now() + Duration::from_secs(5);
            while !started.exists() {
                if Instant::now() >= deadline {
                    std::fs::write(&release, "").unwrap();
                    panic!("first integration did not reach its hook");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            app.integrate(indices[1], "a", None);
            let other = app.integrate_rx.recv_timeout(Duration::from_secs(5));
            std::fs::write(&release, "").unwrap();
            let other = other.expect("a second repository must not wait for the first repository's integration");
            assert_eq!(other.plan, app.plans[indices[1]].id);
            assert!(other.result.is_ok());
            app.finish_integration(other);
            let first = app.integrate_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(first.plan, app.plans[indices[0]].id);
            assert!(first.result.is_ok());
            app.finish_integration(first);
            assert!(app.plans[indices[0]].tasks[0].integrated.is_some());
            assert!(app.plans[indices[1]].tasks[0].integrated.is_some());
        });
    }

    #[test]
    fn approvals_and_held_sends_stay_with_their_plan_across_tab_switches() {
        with_app("app::tests::approvals_and_held_sends_stay_with_their_plan_across_tab_switches", |app, ctx| {
            app.config.agents.push(imacs_core::config::AgentConfig {
                name: "test".into(),
                command: vec![],
                kind: AgentKind::Generic,
                busy_text: Some("esc to interrupt".into()),
            });
            let dir = tempfile::tempdir().unwrap();
            let mut indices = Vec::new();
            for (goal, id) in [("One", "worker-one"), ("Two", "worker-two")] {
                assert!(request(app, json!({"cmd":"plan_set", "goal":goal, "tasks":[{"id":"a", "title":goal}]})).ok);
                let index = app.selected_plan;
                app.plans[index].tasks[0].session = Some(id.into());
                app.plans[index].tasks[0].status = TaskStatus::Running;
                indices.push(index);
                let mut worker = session(id, AgentKind::Generic, AgentStatus::Working, dir.path());
                worker.terminal = Some(
                    Terminal::spawn(
                        ctx,
                        app.palette.clone(),
                        "/bin/cat".into(),
                        vec![],
                        dir.path(),
                        HashMap::new(),
                        100,
                    )
                    .unwrap(),
                );
                app.sessions.push(worker);
            }
            let (one, two) = (indices[0], indices[1]);
            let hook = |session: &str, name: &str| {
                HookMessage {
                pane: Some(session.into()), ancestors: Vec::new(), delivered: 0, approval_reply: None,
                event: imacs_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name":name, "session_id":format!("codex-{session}"), "cwd":dir.path(),
                    "tool_name":"Bash", "tool_input":{"command":"touch permission-fixture", "description":format!("Request from {session}")}
                })).unwrap(),
            }
            };
            let permission = |session: &str| {
                let reply = Arc::new(ipc::ApprovalReply::default());
                let mut event = hook(session, "PermissionRequest");
                event.approval_reply = Some(reply.clone());
                (event, reply)
            };
            app.record_events();
            let (wait_tx, wait_rx) = channel();
            app.control(
                ctx,
                ControlRequest::Wait {
                    sessions: vec!["worker-one".into()],
                    statuses: vec!["needs you".into()],
                    after: Some(app.event_seq),
                    timeout_secs: Some(30),
                },
                wait_tx,
            );
            let (event, second_reply) = permission("worker-two");
            app.on_hook(event);
            assert!(wait_rx.try_recv().is_err(), "another plan's approval must not satisfy this waiter");
            let (event, first_reply) = permission("worker-one");
            app.on_hook(event);
            let ControlOutcome::Done(waited) = wait_rx.recv_timeout(Duration::from_secs(2)).unwrap() else {
                panic!("wait response")
            };
            let event = &waited.result["events"][0];
            assert_eq!(event["plan"], app.plans[one].id);
            assert_eq!(event["task"], "a");
            assert_eq!(event["session"], "worker-one");
            assert_eq!(event["approval"]["command"], "touch permission-fixture");
            assert_eq!(event["approval"]["reason"], "Request from worker-one");
            assert_eq!(event["approval"]["cwd"], json!(dir.path()));
            let other =
                app.events.iter().find(|e| e.session == "worker-two" && e.approval.is_some()).unwrap().to_json();
            assert_eq!(other["plan"], app.plans[two].id);
            assert_eq!(other["task"], "a");
            assert_eq!(other["approval"]["reason"], "Request from worker-two");
            app.last_status.remove("worker-one");
            app.record_events();
            let event = app.events.last().unwrap().to_json();
            assert_eq!(event["plan"], app.plans[one].id);
            assert_eq!(event["task"], "a");
            assert_eq!(event["approval"]["command"], "touch permission-fixture");
            for (index, id) in [(0, "worker-one"), (1, "worker-two")] {
                let response = request(app, json!({"cmd":"send", "session":id, "text":format!("steer-{id}")}));
                assert!(response.ok);
                assert_eq!(response.result["held"], true);
                assert_eq!(response.result["sent"], false);
                let raw = request(app, json!({"cmd":"send", "session":id, "text":format!("raw-{id}\\r"), "raw":true}));
                assert!(!raw.ok);
                assert!(raw.error.unwrap().contains("imacs_approve"));
                assert_eq!(app.sessions[index].held.len(), 1);
                assert!(app.sessions[index].pending.is_none());
            }
            assert_eq!(app.selected_plan, two);
            assert!(request(app, json!({"cmd":"approve", "session":"worker-one", "decision":"allow_always", "message":"First plan approved"})).ok);
            let output: serde_json::Value =
                serde_json::from_str(&first_reply.wait(Duration::ZERO).stdout.unwrap()).unwrap();
            assert_eq!(output["hookSpecificOutput"]["decision"]["behavior"], "allow");
            assert_eq!(output["hookSpecificOutput"]["decision"]["message"], "First plan approved");
            assert!(app.sessions[0].approvals.is_empty());
            assert_eq!(app.sessions[1].approvals.len(), 1);
            assert_eq!(app.sessions[1].status, AgentStatus::NeedsInput);
            assert_eq!(app.sessions[0].held.len(), 1, "approval alone must not flush sends into the permission menu");
            app.on_hook(hook("worker-one", "PostToolUse"));
            assert!(app.sessions[0].held.is_empty());
            assert_eq!(app.sessions[0].pending.as_ref().unwrap().text, "steer-worker-one");
            assert_eq!(app.sessions[1].held.len(), 1);
            assert!(app.sessions[1].pending.is_none());
            let deadline = Instant::now() + Duration::from_secs(2);
            while !app.sessions[0].terminal.as_ref().unwrap().screen_text(20).contains("steer-worker-one") {
                assert!(Instant::now() < deadline, "first plan's plain message was not released");
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(!app.sessions[0].terminal.as_ref().unwrap().screen_text(20).contains("raw-worker-one"));
            assert!(!app.sessions[1].terminal.as_ref().unwrap().screen_text(20).contains("worker-one"));
            app.selected_plan = one;
            app.show_plan_fleet();
            assert!(
                request(
                    app,
                    json!({"cmd":"approve", "session":"worker-two", "decision":"deny", "message":"Second plan denied"})
                )
                .ok
            );
            let output: serde_json::Value =
                serde_json::from_str(&second_reply.wait(Duration::ZERO).stdout.unwrap()).unwrap();
            assert_eq!(output["hookSpecificOutput"]["decision"]["behavior"], "deny");
            assert_eq!(output["hookSpecificOutput"]["decision"]["message"], "Second plan denied");
            assert_eq!(app.sessions[1].held.len(), 1);
            app.on_hook(hook("worker-two", "Interrupt"));
            assert!(app.sessions[1].held.is_empty());
            assert_eq!(app.sessions[1].pending.as_ref().unwrap().text, "steer-worker-two");
            assert_eq!(app.sessions[0].pending.as_ref().unwrap().text, "steer-worker-one");
            let (event, reply) = permission("worker-two");
            app.on_hook(event);
            assert_eq!(app.sessions[1].approvals.len(), 1, "allow_always must not leak to another plan's session");
            assert!(request(app, json!({"cmd":"approve", "session":"worker-two", "decision":"allow"})).ok);
            assert!(reply.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
            let (event, reply) = permission("worker-one");
            app.on_hook(event);
            assert!(reply.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
            assert!(app.sessions[0].approvals.is_empty());
            assert_eq!(app.plans[one].tasks[0].status, TaskStatus::Running);
            assert_eq!(app.plans[two].tasks[0].status, TaskStatus::Running);
        });
    }

    #[test]
    fn multiple_plans_keep_boards_fleets_tools_reports_and_integrations_separate() {
        with_app("app::tests::multiple_plans_keep_boards_fleets_tools_reports_and_integrations_separate", |app, _| {
            let first = request(
                app,
                json!({"cmd":"plan_set", "goal":"First goal", "tasks":[{"id":"a", "title":"First task"}]}),
            );
            assert!(first.ok);
            let first_id = first.result["plan"].as_str().unwrap().to_string();
            let one = app.selected_plan;
            app.plans[one].approved = true;
            app.plans[one].tasks[0].status = TaskStatus::Blocked;
            app.plans[one].tasks[0].session = Some("one-agent".into());
            let second = request(
                app,
                json!({"cmd":"plan_set", "goal":"First goal", "tasks":[{"id":"a", "title":"Second task"}]}),
            );
            assert!(second.ok);
            let second_id = second.result["plan"].as_str().unwrap().to_string();
            assert_ne!(first_id, second_id);
            let two = app.selected_plan;
            assert!(app.plans[one].approved);
            assert!(!app.plans[two].approved);
            assert_eq!(app.plans[two].tasks[0].status, TaskStatus::Planned);
            assert_ne!(app.plans[one].integration_branch(), app.plans[two].integration_branch());
            assert_eq!(app.plans[one].tasks.last().unwrap().id, "review");
            assert_eq!(app.plans[two].tasks.last().unwrap().id, "review");
            app.plans[two].tasks[0].session = Some("two-agent".into());
            app.sessions = vec![
                session("one-agent", AgentKind::Generic, AgentStatus::Working, Path::new("/project")),
                session("two-agent", AgentKind::Generic, AgentStatus::Working, Path::new("/project")),
                session("outside", AgentKind::Generic, AgentStatus::Idle, Path::new("/project")),
                session("chief", AgentKind::Claude, AgentStatus::Idle, Path::new("/project")),
            ];
            app.sessions[3].chief = true;
            app.repos = vec!["/project".into()];
            for (index, visible) in [(one, vec![0]), (two, vec![1])] {
                app.selected_plan = index;
                app.show_plan_fleet();
                assert_eq!(app.visible_sessions(), visible);
                app.toggle_plan();
                assert!(app.panel == PanelMode::Plan);
                app.toggle_plan();
                assert!(app.panel == PanelMode::Diff);
            }
            app.set_view(None);
            assert_eq!(app.visible_sessions(), [0, 1, 2]);
            app.set_view(Some("/project".into()));
            assert_eq!(app.visible_sessions(), [0, 1, 2]);
            app.selected_plan = two;
            app.plans[one].project = Some("/one".into());
            app.plans[two].project = Some("/two".into());
            assert_eq!(app.task_repo(one, &Task::default()), Some("/one".into()));
            assert_eq!(app.task_repo(two, &Task::default()), Some("/two".into()));
            for cmd in ["plan_update", "plan_dispatch", "plan_integrate"] {
                let response = request(app, json!({"cmd":cmd, "id":"a", "status":"done"}));
                let error = response.error.unwrap();
                assert!(error.contains(&first_id) && error.contains(&second_id), "{error}");
            }
            let summary = request(app, json!({"cmd":"plan_get"}));
            assert_eq!(summary.result["plans"].as_array().unwrap().len(), 2);
            assert_eq!(summary.result["plans"][0]["waiting"], json!(["a"]));
            let full = request(app, json!({"cmd":"plan_get", "plan":first_id}));
            assert_eq!(full.result["tasks"][0]["title"], "First task");
            assert_eq!(app.selected_plan, two);
            let update = request(
                app,
                json!({"cmd":"plan_update", "plan":first_id, "id":"a", "status":"done", "note":"verified"}),
            );
            assert!(update.ok);
            assert_eq!(app.plans[one].tasks[0].status, TaskStatus::Done);
            assert_eq!(app.plans[two].tasks[0].status, TaskStatus::Planned);
            assert_eq!(app.selected_plan, two);
            let into = app.plans[one].integration_branch();
            app.integrating.insert((first_id.clone(), "a".into()));
            app.integrating.insert((second_id.clone(), "a".into()));
            app.finish_integration(Integrated {
                plan: first_id.clone(),
                task: "a".into(),
                branch: "first".into(),
                into: into.clone(),
                result: Ok(Integration { committed: true, merge: Merge::Merged, worktree: "/integration".into() }),
                reply: None,
            });
            assert_eq!(app.plans[one].tasks[0].integrated.as_deref(), Some(into.as_str()));
            assert!(app.plans[two].tasks[0].integrated.is_none());
            assert!(app.integrating.contains(&(second_id.clone(), "a".into())));
            assert_eq!(app.report(Some("one-agent"), "done", "first report"), Some("a".into()));
            assert_eq!(app.plans[one].tasks[0].status, TaskStatus::Review);
            assert!(app.plans[two].tasks[0].report.is_none());
            let waited = request(app, json!({"cmd":"wait", "after":0, "statuses":["reported"]}));
            assert_eq!(waited.result["events"][0]["plan"], first_id);
            assert_eq!(waited.result["events"][0]["task"], "a");
            app.record_events();
            for (session, plan) in [("one-agent", &first_id), ("two-agent", &second_id)] {
                let event = app.events.iter().find(|e| e.session == session && e.report.is_none()).unwrap().to_json();
                assert_eq!(event["plan"], *plan);
                assert_eq!(event["task"], "a");
            }
            app.sessions[0].status = AgentStatus::Exited;
            app.plans[one].tasks[0].status = TaskStatus::Running;
            app.sync_plan();
            assert_eq!(app.plans[one].tasks[0].status, TaskStatus::Blocked);
            assert_eq!(app.plans[two].tasks[0].status, TaskStatus::Planned);
            let old = app.plans[two].clone();
            assert!(request(app, json!({"cmd":"plan_set", "plan":first_id, "goal":"First goal", "tasks":[{"id":"a", "title":"Revised"}]})).ok);
            assert_eq!(app.plans[two], old);
            assert_eq!(app.plans[one].tasks[0].session.as_deref(), Some("one-agent"));
            let loaded = plan::load_plans(&paths::imacs_home()).unwrap().0;
            assert_eq!(loaded.iter().find(|p| p.id == first_id).unwrap(), &app.plans[one]);
            assert_eq!(loaded.iter().find(|p| p.id == second_id).unwrap().tasks[0].title, "Second task");
        });
    }

    #[test]
    fn plan_set_rewords_migrated_plan_without_losing_progress_and_creates_fresh_plans() {
        with_app(
            "app::tests::plan_set_rewords_migrated_plan_without_losing_progress_and_creates_fresh_plans",
            |app, _| {
                let mut legacy = Plan::default();
                legacy.replace(
                    "Ship the API".into(),
                    vec![Task { id: "a".into(), title: "Build API".into(), ..Task::default() }],
                );
                legacy.approved = true;
                legacy.integration = Some("imacs/already-integrated".into());
                for task in &mut legacy.tasks {
                    task.status = if task.id == "a" { TaskStatus::Running } else { TaskStatus::Blocked };
                    task.session = Some(format!("agent-{}", task.id));
                    task.branch = Some(format!("imacs/existing-{}", task.id));
                    task.repo = Some("/project".into());
                    task.notes = vec!["Existing note".into()];
                    task.report = Some(Report { status: "progress".into(), summary: "Existing report".into(), at: 42 });
                    task.integrated = legacy.integration.clone();
                }
                legacy.save(&paths::plan_file()).unwrap();
                app.plans = plan::load_plans(&paths::imacs_home()).unwrap().0;
                app.selected_plan = 0;
                let id = app.plans[0].id.clone();
                let progress = app.plans[0].tasks.clone();
                let tasks =
                    json!([{"id":"a", "title":"Revised API task"}, {"id":"review", "title":"Review revised plan"}]);
                let updated =
                    request(app, json!({"cmd":"plan_set", "plan":id, "goal":"Deliver the API", "tasks":tasks}));
                assert!(updated.ok);
                assert_eq!(app.plans[0].goal, "Deliver the API");
                assert!(!app.plans[0].approved);
                assert_eq!(app.plans[0].integration, legacy.integration);
                assert_eq!(updated.result["integration"], "imacs/already-integrated");
                for (task, old) in app.plans[0].tasks.iter().zip(&progress) {
                    assert_eq!(task.status, old.status);
                    assert_eq!(task.session, old.session);
                    assert_eq!(task.branch, old.branch);
                    assert_eq!(task.repo, old.repo);
                    assert_eq!(task.notes, old.notes);
                    assert_eq!(task.report, old.report);
                    assert_eq!(task.integrated, old.integrated);
                }
                let original = app.plans[0].clone();
                for goal in ["Deliver the API", "Another goal"] {
                    let created = request(app, json!({"cmd":"plan_set", "goal":goal, "tasks":tasks}));
                    assert!(created.ok);
                    let fresh = &app.plans[app.selected_plan];
                    assert_ne!(fresh.id, id);
                    assert!(!fresh.approved);
                    assert!(fresh.integration.is_none());
                    assert_ne!(fresh.integration_branch(), original.integration_branch());
                    for task in &fresh.tasks {
                        assert_eq!(task.status, TaskStatus::Planned);
                        assert!(task.session.is_none());
                        assert!(task.branch.is_none());
                        assert!(task.notes.is_empty());
                        assert!(task.report.is_none());
                        assert!(task.integrated.is_none());
                    }
                    assert_eq!(app.plans[0], original);
                }
                let loaded = plan::load_plans(&paths::imacs_home()).unwrap().0;
                assert_eq!(loaded.len(), 3);
                assert_eq!(loaded.iter().find(|p| p.id == id).unwrap(), &original);
            },
        );
    }

    #[test]
    fn clicking_plan_tabs_and_approving_boards_targets_only_the_selected_plan() {
        with_app("app::tests::clicking_plan_tabs_and_approving_boards_targets_only_the_selected_plan", |app, ctx| {
            for goal in ["Atlas", "Beacon"] {
                assert!(
                    request(
                        app,
                        json!({"cmd":"plan_set", "goal":goal, "tasks":[
                    {"id":"a", "title":format!("{goal} task"), "brief":format!("{goal} instructions")}]})
                    )
                    .ok
                );
            }
            let one = app.plans.iter().position(|p| p.goal == "Atlas").unwrap();
            let two = app.selected_plan;
            app.plans.push(Plan {
                id: "archived".into(),
                goal: "Archived".into(),
                status: plan::PlanStatus::Archived,
                ..Plan::default()
            });
            app.plans.push(Plan {
                id: "completed".into(),
                goal: "Completed".into(),
                status: plan::PlanStatus::Completed,
                ..Plan::default()
            });
            for (index, id) in [(one, "atlas-agent"), (two, "beacon-agent")] {
                app.plans[index].tasks[0].session = Some(id.into());
                app.sessions.push(session(id, AgentKind::Generic, AgentStatus::Working, Path::new("/project")));
            }
            let mut time = 0.0;
            let mut frame = |app: &mut App, events| {
                time += 0.05;
                let output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1700.0, 1000.0))),
                        time: Some(time),
                        events,
                        ..Default::default()
                    },
                    |ui| {
                        egui::Panel::top("test-top").exact_size(34.0).show(ui, |ui| app.top_bar(ui));
                        egui::CentralPanel::default().show(ui, |ui| {
                            if app.panel == PanelMode::Plan {
                                app.plan_panel(ui, ui.max_rect());
                            }
                        });
                    },
                );
                let text: Vec<_> = output
                    .shapes
                    .iter()
                    .filter_map(|shape| match &shape.shape {
                        egui::epaint::Shape::Text(t) => {
                            Some((t.galley.text().to_string(), t.pos + t.galley.rect.center().to_vec2()))
                        }
                        _ => None,
                    })
                    .collect();
                output.drop_without_applying_deltas();
                text
            };
            let button = |pos, pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            for (goal, index, visible) in [("Atlas", one, 0), ("Beacon", two, 1)] {
                let text = frame(app, vec![]);
                assert!(!text.iter().any(|(t, _)| t.contains("● Archived") || t.contains("● Completed")));
                let pos = text.iter().find(|(t, _)| t.starts_with(&format!("● {goal} ·"))).unwrap().1;
                frame(app, vec![egui::Event::PointerMoved(pos), button(pos, true)]);
                frame(app, vec![button(pos, false)]);
                assert_eq!(app.selected_plan, index);
                assert!(app.plan_view);
                assert_eq!(app.visible_sessions(), [visible]);
                app.toggle_plan();
                let text = frame(app, vec![]);
                assert!(text.iter().any(|(t, _)| t == &format!("{goal} task")));
                let other = if goal == "Atlas" { "Beacon task" } else { "Atlas task" };
                assert!(!text.iter().any(|(t, _)| t == other));
                assert!(!text.iter().any(|(t, _)| t == &format!("{goal} instructions")));
                let task_pos = text.iter().find(|(t, _)| t == &format!("{goal} task")).unwrap().1;
                frame(app, vec![egui::Event::PointerMoved(task_pos), button(task_pos, true)]);
                frame(app, vec![button(task_pos, false)]);
                let text = frame(app, vec![]);
                assert!(text.iter().any(|(t, _)| t == &format!("{goal} instructions")));
                let approve = text.iter().find(|(t, _)| t == "Approve plan").unwrap().1;
                frame(app, vec![egui::Event::PointerMoved(approve), button(approve, true)]);
                frame(app, vec![button(approve, false)]);
                assert!(app.plans[index].approved);
                if index == one {
                    assert!(!app.plans[two].approved);
                }
            }
            let text = frame(app, vec![]);
            let archive = text.iter().find(|(t, _)| t == "Archive plan").unwrap().1;
            frame(app, vec![egui::Event::PointerMoved(archive), button(archive, true)]);
            frame(app, vec![button(archive, false)]);
            assert_eq!(app.plans[two].status, PlanStatus::Archived);
            assert_eq!(app.selected_plan, one);
            let text = frame(app, vec![]);
            assert!(!text.iter().any(|(t, _)| t.starts_with("● Beacon ·")));
            app.toggle_plan();
            let text = frame(app, vec![]);
            let complete = text.iter().find(|(t, _)| t == "Complete plan").unwrap().1;
            frame(app, vec![egui::Event::PointerMoved(complete), button(complete, true)]);
            frame(app, vec![button(complete, false)]);
            assert_eq!(app.plans[one].status, PlanStatus::Completed);
            assert!(app.plans.iter().all(|p| !p.is_open()));
            assert_eq!(app.sessions.len(), 2, "closing plans does not terminate their sessions");
        });
    }

    #[test]
    fn background_dispatch_preserves_selection_and_routes_delayed_start_failure() {
        with_app("app::tests::background_dispatch_preserves_selection_and_routes_delayed_start_failure", |app, ctx| {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path().join("repo");
            std::fs::create_dir(&repo).unwrap();
            git::git(&repo, &["init", "-q", "-b", "main"]).unwrap();
            git::git(
                &repo,
                &[
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.org",
                    "commit",
                    "--allow-empty",
                    "-qm",
                    "initial",
                ],
            )
            .unwrap();
            app.config.worktree_path = format!("{}/{{slug}}", dir.path().display());
            let mut indices = Vec::new();
            for goal in ["One", "Two"] {
                assert!(
                    request(
                        app,
                        json!({"cmd":"plan_set", "goal":goal, "tasks":[
                    {"id":"a", "title":"Same task", "repo":repo, "agent":"missing-test-agent"}]})
                    )
                    .ok
                );
                indices.push(app.selected_plan);
            }
            let (one, two) = (indices[0], indices[1]);
            assert!(app.dispatch(one, Some("a")).unwrap_err().contains("not approved"));
            app.plans[one].approved = true;
            assert_eq!(app.dispatch(one, Some("a")).unwrap(), ["a"]);
            assert_eq!(app.selected_plan, two);
            assert!(app.plan_view);
            assert!(app.changed_plans.contains(&app.plans[one].id));
            app.selected_plan = two;
            app.show_plan_fleet();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !app.creating.is_empty() && Instant::now() < deadline {
                app.pump(ctx);
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(app.creating.is_empty());
            assert_eq!(app.selected_plan, two);
            assert_eq!(app.plans[one].tasks[0].status, TaskStatus::Blocked);
            assert!(app.plans[one].tasks[0].notes.last().unwrap().contains("could not start"));
            assert_eq!(app.plans[two].tasks[0].status, TaskStatus::Planned);
            assert!(app.plans[two].tasks[0].notes.is_empty());
        });
    }

    fn permission_hooks_wake_waiters_and_accept_decisions(app: &mut App, ctx: &egui::Context) {
        let dir = tempfile::tempdir().unwrap();
        app.sessions = vec![session("codex", AgentKind::Generic, AgentStatus::Working, dir.path())];
        let permission = |command: &str, reason: &str| {
            let response = Arc::new(ipc::ApprovalReply::default());
            (
                HookMessage {
                    pane: Some("codex".into()),
                    ancestors: Vec::new(),
                    event: imacs_core::hook::HookEvent::from_payload(&json!({
                        "hook_event_name":"PermissionRequest", "session_id":"s1", "cwd":dir.path(),
                        "tool_name":"Bash", "tool_input":{"command":command,"description":reason}
                    }))
                    .unwrap(),
                    delivered: 0,
                    approval_reply: Some(response.clone()),
                },
                response,
            )
        };
        for decision in [Decision::Allow, Decision::Deny, Decision::AllowAlways] {
            let (wait_tx, wait_rx) = channel();
            app.control(
                ctx,
                ControlRequest::Wait {
                    sessions: vec!["codex".into()],
                    statuses: vec!["needs you".into()],
                    after: Some(app.event_seq),
                    timeout_secs: Some(30),
                },
                wait_tx,
            );
            assert!(wait_rx.try_recv().is_err());
            let (hook, hook_rx) = permission("touch approval-file", "Create a file");
            app.on_hook(hook);
            assert_eq!(app.sessions[0].status, AgentStatus::NeedsInput);
            assert!(app.sessions[0].hooks_active);
            let ControlOutcome::Done(response) = wait_rx.recv().unwrap() else { panic!("wait response") };
            assert_eq!(response.result["events"][0]["approval"]["command"], "touch approval-file");
            assert_eq!(response.result["events"][0]["approval"]["reason"], "Create a file");
            assert_eq!(response.result["events"][0]["approval"]["cwd"], json!(dir.path()));
            let (tx, rx) = channel();
            app.control(
                ctx,
                ControlRequest::Approve { session: "codex".into(), decision, message: Some("Reviewed".into()) },
                tx,
            );
            assert!(matches!(rx.recv().unwrap(), ControlOutcome::Done(response) if response.ok));
            let output: serde_json::Value =
                serde_json::from_str(&hook_rx.wait(Duration::ZERO).stdout.unwrap()).unwrap();
            assert_eq!(
                output["hookSpecificOutput"]["decision"]["behavior"],
                if decision == Decision::Deny { "deny" } else { "allow" }
            );
            assert!(app.sessions[0].approvals.is_empty());
        }
        let (hook, rx) = permission("touch approval-file", "A different reason");
        app.on_hook(hook);
        assert!(rx.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
        assert!(app.sessions[0].approvals.is_empty());
        let (hook, rx) = permission("git status", "Check code");
        app.on_hook(hook);
        assert!(rx.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
        let (hook, rx) = permission("touch another-file", "Another request");
        app.on_hook(hook);
        assert!(rx.wait(Duration::ZERO).stdout.is_none());
        assert!(app.approve(0, Decision::Allow, None).is_err());
        assert_eq!(app.sessions[0].approvals.len(), 1);
        let event = imacs_core::hook::HookEvent::from_payload(&json!({
            "hook_event_name":"PostToolUse", "session_id":"s1", "cwd":dir.path(),
            "tool_name":"Bash", "tool_input":{"command":"touch another-file"}
        }))
        .unwrap();
        app.on_hook(HookMessage {
            pane: Some("codex".into()),
            ancestors: Vec::new(),
            event,
            delivered: 0,
            approval_reply: None,
        });
        assert!(app.sessions[0].approvals.is_empty());
        assert_eq!(app.sessions[0].status, AgentStatus::Working);
        assert!(app.approve(0, Decision::Deny, None).is_err());
        assert!(CHIEF_ALLOWED_TOOLS.contains(&"mcp__imacs__imacs_approve"));
    }

    fn busy_text_is_only_a_fallback(app: &mut App, ctx: &egui::Context) {
        app.config.agents.push(imacs_core::config::AgentConfig {
            name: "test".into(),
            command: vec![],
            kind: AgentKind::Generic,
            busy_text: Some("esc to interrupt".into()),
        });
        let dir = tempfile::tempdir().unwrap();
        let terminal = Terminal::spawn(
            ctx,
            app.palette.clone(),
            "/bin/sh".into(),
            vec!["-c".into(), "printf 'esc to interrupt'; sleep 10".into()],
            dir.path(),
            HashMap::new(),
            100,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !terminal.screen_text(12).contains("esc to interrupt") {
            assert!(Instant::now() < deadline, "terminal did not produce busy text");
            std::thread::sleep(Duration::from_millis(10));
        }
        let process_id = terminal.process_id();
        app.sessions = vec![
            session("fallback", AgentKind::Generic, AgentStatus::Idle, dir.path()),
            session("same-cwd", AgentKind::Generic, AgentStatus::Idle, dir.path()),
        ];
        app.sessions[0].terminal = Some(terminal);
        app.busy_checked = Instant::now() - Duration::from_secs(1);
        app.track_busy();
        assert_eq!(app.sessions[0].status, AgentStatus::Working);
        let event = imacs_core::hook::HookEvent::from_payload(&json!({
            "hook_event_name":"SessionStart", "session_id":"mapped-codex-session", "cwd":dir.path(),
        }))
        .unwrap();
        app.on_hook(HookMessage { pane: None, ancestors: vec![process_id], event, delivered: 0, approval_reply: None });
        assert_eq!(app.sessions.len(), 2);
        assert_eq!(app.sessions[0].claude_id.as_deref(), Some("mapped-codex-session"));
        assert_eq!(app.sessions[1].claude_id.as_deref(), Some("same-cwd"));
        app.sessions[0].status = AgentStatus::Done;
        app.busy_checked = Instant::now() - Duration::from_secs(1);
        app.track_busy();
        assert_eq!(app.sessions[0].status, AgentStatus::Done);
        let event = imacs_core::hook::HookEvent::from_payload(&json!({
            "hook_event_name":"PermissionRequest", "session_id":"mapped-codex-session", "cwd":dir.path(),
            "tool_name":"Bash", "tool_input":{"command":"touch fixture", "description":"test"}
        }))
        .unwrap();
        let response = Arc::new(ipc::ApprovalReply::default());
        app.on_hook(HookMessage { pane: None, ancestors: vec![], event, delivered: 0, approval_reply: Some(response) });
        assert_eq!(app.sessions.len(), 2);
        assert_eq!(app.sessions[0].status, AgentStatus::NeedsInput);
        let before = app.sessions[0].terminal.as_ref().unwrap().screen_text(12);
        app.sessions[0].pending = Some(Pending {
            text: "previous steering message".into(),
            sent: Instant::now() - DELIVERY_TIMEOUT,
            attempts: 1,
            was_working: true,
        });
        app.verify_deliveries();
        assert_eq!(app.sessions[0].pending.as_ref().unwrap().attempts, 1);
        let response = request(app, json!({"cmd":"send", "session":"fallback", "text":"held steering message"}));
        assert_eq!(response.result["held"], true);
        assert_eq!(response.result["sent"], false);
        let rejected = request(app, json!({"cmd":"send", "session":"fallback", "text":"y", "raw":true}));
        assert!(!rejected.ok);
        assert!(rejected.error.unwrap().contains("imacs_approve"));
        assert_eq!(app.sessions[0].held, ["held steering message"]);
        app.active = Some("fallback".into());
        app.steer_text = "steer while waiting".into();
        app.send_steer();
        assert!(app.toast.as_ref().unwrap().0.starts_with("Held for 1"));
        let root = app.sessions[0].root();
        app.diffs.entry(root.clone()).or_default().comments.push(steer::ReviewComment {
            path: "fixture.rs".into(),
            line: Some(1),
            removed: false,
            excerpt: "line".into(),
            body: "review while waiting".into(),
        });
        let review = request(app, json!({"cmd":"send_review", "session":"fallback"}));
        assert_eq!(review.result["held"], true);
        assert_eq!(review.result["held_comments"], 1);
        assert!(review.result.get("sent_comments").is_none());
        assert!(app.toast.as_ref().unwrap().0.starts_with("Held 1 comment"));
        assert_eq!(app.sessions[0].held.len(), 3);

        assert_eq!(app.sessions[0].terminal.as_ref().unwrap().screen_text(12), before);
        app.approve(0, Decision::Allow, None).unwrap();
        assert_eq!(app.sessions[0].held.len(), 3);
        assert_eq!(app.sessions[0].terminal.as_ref().unwrap().screen_text(12), before);
        let event = imacs_core::hook::HookEvent::from_payload(&json!({
            "hook_event_name":"PostToolUse", "session_id":"mapped-codex-session", "cwd":dir.path(),
            "tool_name":"Bash", "tool_input":{"command":"touch fixture"}
        }))
        .unwrap();
        app.on_hook(HookMessage { pane: None, ancestors: vec![], event, delivered: 0, approval_reply: None });
        assert!(app.sessions[0].held.is_empty());
        let delivered = &app.sessions[0].pending.as_ref().unwrap().text;
        assert!(delivered.contains("held steering message"));
        assert!(delivered.contains("steer while waiting"));
        assert!(delivered.contains("review while waiting"));
        app.sessions.clear();
    }

    fn close_idle_leaves_shells_alone(app: &mut App) {
        let project = Path::new("/project");
        app.repos = vec![project.into()];
        app.sessions = vec![
            session("shell", AgentKind::Shell, AgentStatus::Idle, project),
            session("exited-shell", AgentKind::Shell, AgentStatus::Exited, project),
            session("idle", AgentKind::Generic, AgentStatus::Idle, project),
            session("exited", AgentKind::Claude, AgentStatus::Exited, project),
            session("working", AgentKind::Generic, AgentStatus::Working, project),
            session("done", AgentKind::Claude, AgentStatus::Done, project),
            session("chief", AgentKind::Claude, AgentStatus::Idle, project),
            session("in-plan", AgentKind::Claude, AgentStatus::Idle, project),
            session("elsewhere", AgentKind::Claude, AgentStatus::Idle, Path::new("/other")),
        ];
        app.sessions[6].chief = true;
        app.plans[app.selected_plan].tasks = vec![Task { session: Some("in-plan".into()), ..Task::default() }];
        assert_eq!(app.close_idle(project), 2);
        assert_eq!(
            app.sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["shell", "exited-shell", "working", "done", "chief", "in-plan", "elsewhere"]
        );
    }

    fn voice_to_a_shell_stays_in_the_steer_bar(app: &mut App, ctx: &egui::Context) {
        app.config.voice.target = VoiceTarget::Active;
        app.active = Some("shell".into());
        assert!(app.sessions[0].can_receive());
        app.steer_text = "existing draft".into();
        app.send_voice(ctx, "delete everything");
        assert_eq!(app.steer_text, "existing draft\ndelete everything");
        assert!(app.focus_steer);
        assert!(app.sessions[0].pending.is_none());
    }

    fn plan_set_resolves_cached_worktree_projects(app: &mut App) {
        let project = PathBuf::from("/project");
        app.worktree_project.insert("/worktree".into(), project.clone());
        let (tx, rx) = channel();
        app.control(
            &app.ctx.clone(),
            ControlRequest::PlanSet {
                plan: None,
                goal: "ship".into(),
                tasks: vec![Task { id: "a".into(), repo: Some("/worktree/src".into()), ..Task::default() }],
                integration: None,
            },
            tx,
        );
        assert!(matches!(rx.recv().unwrap(), ControlOutcome::Done(response) if response.ok));
        assert_eq!(app.plans[app.selected_plan].project, Some(project));
    }

    fn dispatch_requires_integrated_dependencies(app: &mut App) {
        let plan_id = app.plans[app.selected_plan].id.clone();
        app.plans[app.selected_plan] = Plan {
            id: plan_id,
            approved: true,
            tasks: vec![
                Task { id: "a".into(), status: TaskStatus::Done, ..Task::default() },
                Task { id: "b".into(), depends_on: vec!["a".into()], ..Task::default() },
            ],
            ..Plan::default()
        };
        for only in [Some("b"), None] {
            let error = app.dispatch(app.selected_plan, only).unwrap_err();
            assert!(error.contains("imacs_plan_integrate"), "{error}");
            assert!(error.contains("a"));
            assert!(app.creating.is_empty());
        }
        let into = app.plans[app.selected_plan].integration_branch();
        for result in [
            Err("failed".into()),
            Ok(Integration {
                committed: false,
                merge: Merge::Conflict(vec!["file".into()]),
                worktree: "/integration".into(),
            }),
        ] {
            app.finish_integration(Integrated {
                plan: app.plans[app.selected_plan].id.clone(),
                task: "a".into(),
                branch: "task-a".into(),
                into: into.clone(),
                result,
                reply: None,
            });
            assert!(app.plans[app.selected_plan].tasks[0].integrated.is_none());
            assert!(app.plans[app.selected_plan].ready().is_empty());
        }
        app.plans[app.selected_plan].tasks[0].status = TaskStatus::Done;
        app.finish_integration(Integrated {
            plan: app.plans[app.selected_plan].id.clone(),
            task: "a".into(),
            branch: "task-a".into(),
            into: into.clone(),
            result: Ok(Integration { committed: true, merge: Merge::Merged, worktree: "/integration".into() }),
            reply: None,
        });
        assert_eq!(app.plans[app.selected_plan].tasks[0].integrated, Some(into));
        assert_eq!(app.plans[app.selected_plan].ready().iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["b"]);
        let loaded =
            Plan::load(&paths::imacs_home().join("plans").join(format!("{}.json", app.plans[app.selected_plan].id)))
                .unwrap();
        assert_eq!(loaded.tasks[0].integrated, app.plans[app.selected_plan].tasks[0].integrated);
    }
}

#[cfg(test)]
mod chief_tests {
    use super::*;

    #[test]
    fn divider_drags_and_double_click_resets() {
        let ctx = egui::Context::default();
        let palette = Palette::from_config(&imacs_core::config::ThemeConfig::default());
        let mut width = Some(720.0);
        let mut time = 0.0;
        let mut frame = |events| {
            time += 0.05;
            let mut save = false;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(2000.0, 1000.0))),
                time: Some(time),
                events,
                ..Default::default()
            };
            let output = ctx.run_ui(input, |ui| {
                egui::Panel::top("top").exact_size(34.0).show(ui, |_| {});
                egui::Panel::left("sidebar").exact_size(250.0).resizable(false).show(ui, |_| {});
                let rect = egui::Panel::left("chief-dock")
                    .exact_size(width.unwrap_or(595.0))
                    .resizable(false)
                    .frame(egui::Frame::new().inner_margin(egui::Margin::same(6)))
                    .show(ui, |ui| {
                        ui.interact(ui.max_rect(), egui::Id::NULL.with("chief-term"), Sense::click_and_drag());
                        ui.take_available_space();
                    })
                    .response
                    .rect;
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.interact(ui.max_rect(), egui::Id::NULL.with("agent-term"), Sense::click_and_drag());
                });
                save = chief_divider(ui, rect, &mut width, &palette);
            });
            output.drop_without_applying_deltas();
            (width, save)
        };
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let start = pos2(970.0, 400.0);
        let end = pos2(1100.0, 400.0);
        frame(vec![]);
        frame(vec![egui::Event::PointerMoved(start), button(start, true)]);
        assert_eq!(frame(vec![egui::Event::PointerMoved(end)]).0, Some(850.0));
        assert!(frame(vec![button(end, false)]).1);
        frame(vec![]);
        frame(vec![button(end, true)]);
        frame(vec![button(end, false)]);
        frame(vec![button(end, true)]);
        assert_eq!(frame(vec![button(end, false)]), (None, true));
    }
}
