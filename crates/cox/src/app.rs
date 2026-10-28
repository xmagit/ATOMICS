use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

use cox_core::config::{AgentKind, Config};
use cox_core::git::{self, Worktree, WorktreeEntry};
use cox_core::layout::{self, DiffPlacement, Layout};
use cox_core::session::{AgentStatus, pick_tiled, title_for};
use cox_core::store::{SessionRecord, Store};
use cox_core::{paths, steer};
use egui::{Color32, FontId, Key, Rect, RichText, Sense, Stroke, Vec2, pos2, vec2};
use notify_debouncer_mini::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer};

use crate::diff_view::{self, DiffAction, DiffState, DiffWorker, ExplorerState, Header, WorkerResult};
use crate::highlight::Highlighter;
use crate::ipc::{self, Channels, ControlMessage, ControlOutcome, HookMessage, Hub};
use crate::terminal::{Metrics, Terminal};
use crate::theme::{Palette, lerp, shortcut};
use cox_core::control::{ControlRequest, ControlResponse};
use cox_core::plan::{Plan, Report, Task, TaskStatus};
use cox_core::workspace::{self, Workspace};
use serde_json::json;

const TAB_HEIGHT: f32 = 30.0;
const TILE_HEADER: f32 = 26.0;
const WORKTREE_REFRESH: Duration = Duration::from_secs(30);
const BACKGROUND_REFRESH: Duration = Duration::from_secs(5);

/// Folders whose churn (builds, dependencies, git internals) never changes the diff.
const IGNORED_DIRS: &[&str] = &[".git", "node_modules", "target", "bin", "obj", ".idea", ".vs", "dist"];

#[derive(Default)]
pub struct Options {
    /// Projects to open, from `cox <path>`; empty means the launch folder or the last session.
    pub projects: Vec<PathBuf>,
    pub workspace: Option<String>,
    pub open: Vec<(String, PathBuf)>,
    pub screenshot: Option<PathBuf>,
    pub screenshot_delay: f32,
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
    pub selected: bool,
    /// A message typed into the terminal that the agent has not visibly picked up yet.
    pending: Option<Pending>,
}

struct Pending {
    text: String,
    sent: Instant,
    attempts: u8,
    was_working: bool,
}

const DELIVERY_TIMEOUT: Duration = Duration::from_secs(4);
const DELIVERY_ATTEMPTS: u8 = 3;

impl Session {
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
    task: Option<String>,
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
    /// Set for a `cox_report` from the agent: (status, summary, task).
    report: Option<(String, String, Option<String>)>,
}

impl StatusEvent {
    /// What `wait` matches on: the agent's status, or "reported".
    fn kind(&self) -> &str {
        if self.report.is_some() { "reported" } else { self.status.label() }
    }

    fn to_json(&self) -> serde_json::Value {
        let mut value = json!({ "seq": self.seq, "session": self.session, "title": self.title, "status": self.kind(), "at": self.at });
        if let Some((status, summary, task)) = &self.report {
            value["report"] = json!({ "status": status, "summary": summary, "task": task });
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
    active: Option<String>,
    diff_open: bool,
    /// Worktree picked in the sidebar without an agent; the diff otherwise follows the active session.
    diff_root: Option<PathBuf>,
    diffs: HashMap<PathBuf, DiffState>,
    worker: DiffWorker,
    hub: Arc<Mutex<Hub>>,
    hook_rx: Receiver<HookMessage>,
    control_rx: Receiver<ControlMessage>,
    hook_error: Option<String>,
    busy_checked: Instant,
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
    plan: Plan,
    expanded_tasks: HashSet<String>,
    worktree_tx: Sender<(String, Result<PathBuf, String>, Launch)>,
    worktree_rx: Receiver<(String, Result<PathBuf, String>, Launch)>,
    creating: Vec<String>,
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
    watcher: Option<(PathBuf, Debouncer<RecommendedWatcher>)>,
    watch_tx: Sender<PathBuf>,
    watch_rx: Receiver<PathBuf>,
    toast: Option<(String, Instant)>,
    screenshot: Option<Screenshot>,
    pending_open: Vec<(String, PathBuf)>,
    resume: Vec<SessionRecord>,
    launch_dir: PathBuf,
}

/// The PATH of an interactive login shell. Apps started from the Dock get a bare
/// PATH, which would hide `claude`, `codex` and everything installed by Homebrew.
static SHELL_PATH: LazyLock<Option<String>> = LazyLock::new(|| {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let out = std::process::Command::new(shell)
        .args(["-l", "-i", "-c", "printf '__COX__%s__COX__' \"$PATH\""])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let start = text.find("__COX__")? + 7;
    let end = text[start..].find("__COX__")? + start;
    Some(text[start..end].to_string())
});

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

        if let Ok(exe) = std::env::current_exe() {
            let settings = cox_core::hook::claude_settings(&exe);
            let file = paths::hook_settings_file();
            let _ = std::fs::create_dir_all(paths::cox_home());
            let _ = std::fs::write(&file, serde_json::to_vec_pretty(&settings).unwrap_or_default());
        }

        let hub = Arc::new(Mutex::new(Hub::default()));
        let (hook_tx, hook_rx) = channel();
        let (control_tx, control_rx) = channel();
        let channels = Channels { hooks: hook_tx, control: control_tx };
        let hook_error =
            ipc::serve(&paths::socket_path(), hub.clone(), channels, ctx.clone()).err().map(|e| format!("{e:#}"));
        let (watch_tx, watch_rx) = channel();
        let (worktree_tx, worktree_rx) = channel();
        let launch_dir = std::env::current_dir().unwrap_or_else(|_| paths::home());

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
            active: None,
            diff_open: true,
            diff_root: None,
            diffs: HashMap::new(),
            worker: DiffWorker::new(ctx.clone()),
            hub,
            hook_rx,
            control_rx,
            hook_error,
            busy_checked: Instant::now(),
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
            plan: Plan::load(&paths::plan_file()).unwrap_or_default(),
            expanded_tasks: HashSet::new(),
            worktree_tx,
            worktree_rx,
            creating: Vec::new(),
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
            watch_tx,
            watch_rx,
            toast: None,
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
        app
    }

    /// `cox <path>` or `--workspace`, else the repository cox was started in, else
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

    fn create_worktree(&mut self, repo: PathBuf, branch: String, launch: Launch) {
        let path = git::worktree_path(&self.config.worktree_path, &repo, &branch);
        let (tx, base_ref, ctx) = (self.worktree_tx.clone(), self.config.base_ref.clone(), self.ctx.clone());
        self.creating.push(branch.clone());
        self.toast(format!("Creating worktree {}…", paths::display(&path)));
        std::thread::spawn(move || {
            let result =
                git::add_worktree(&repo, &path, &branch, &base_ref).map(|()| path).map_err(|e| format!("{e:#}"));
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
        let Some(agent) = self.config.agent(agent_name).cloned() else {
            return Err(format!("No agent named {agent_name} in the config"));
        };
        if !cwd.is_dir() {
            return Err(format!("{} is not a folder", cwd.display()));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let mut env: HashMap<String, String> = [
            ("COX_PANE", id.clone()),
            ("COX_SOCKET", paths::socket_path().to_string_lossy().into_owned()),
            ("TERM", "xterm-256color".into()),
            ("COLORTERM", "truecolor".into()),
            ("TERM_PROGRAM", "cox".into()),
        ]
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
        // Every agent cox starts can report back to the plan with `cox_report`.
        let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "cox".into());
        let socket = paths::socket_path().to_string_lossy().into_owned();
        match agent.kind {
            AgentKind::Claude => {
                let config = json!({ "mcpServers": { "cox": {
                    "command": exe, "args": ["mcp", "--agent"], "env": { "COX_PANE": id, "COX_SOCKET": socket }
                } } });
                args.extend([
                    "--mcp-config".to_string(),
                    config.to_string(),
                    "--allowedTools".to_string(),
                    "mcp__cox__cox_report".to_string(),
                ]);
            }
            AgentKind::Generic if agent.command.first().is_some_and(|c| c.ends_with("codex")) => {
                let quoted = |text: &str| serde_json::to_string(text).unwrap_or_default();
                args.extend([
                    "-c".to_string(),
                    format!("mcp_servers.cox.command={}", quoted(&exe)),
                    "-c".to_string(),
                    "mcp_servers.cox.args=[\"mcp\", \"--agent\"]".to_string(),
                    "-c".to_string(),
                    format!("mcp_servers.cox.env={{ COX_PANE = {}, COX_SOCKET = {} }}", quoted(&id), quoted(&socket)),
                    "-c".to_string(),
                    "mcp_servers.cox.default_tools_approval_mode=\"approve\"".to_string(),
                ]);
            }
            _ => {}
        }
        if agent.kind == AgentKind::Claude {
            args.push("--settings".into());
            args.push(paths::hook_settings_file().to_string_lossy().into_owned());
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
        let shell_prompt = match (&prompt, agent.kind) {
            (Some(prompt), AgentKind::Shell) => Some(prompt.clone()),
            (Some(prompt), _) => {
                args.push(prompt.clone());
                None
            }
            _ => None,
        };
        let program = resolve_program(&program);
        match Terminal::spawn(ctx, self.palette.clone(), program, args, &cwd, env) {
            Ok(terminal) => {
                let worktree = git::worktree_of(&cwd);
                let title = title_for(worktree.as_ref().and_then(|w| w.branch.as_deref()), &cwd);
                self.sessions.push(Session {
                    id: id.clone(),
                    agent: agent.name.clone(),
                    kind: agent.kind,
                    terminal: Some(terminal),
                    claude_id,
                    cwd,
                    worktree,
                    title,
                    status: if agent.kind == AgentKind::Shell { AgentStatus::Idle } else { AgentStatus::Starting },
                    last_prompt: None,
                    last_message: None,
                    selected: false,
                    pending: None,
                });
                if let Some(text) = shell_prompt
                    && let Some(terminal) = self.sessions.last().and_then(|s| s.terminal.as_ref())
                {
                    terminal.submit(&text);
                }
                self.activate(&id, false);
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
                "files": p.diff.files.len(), "additions": p.diff.additions(), "deletions": p.diff.deletions()
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
            "outside_cox": s.terminal.is_none(),
            "claude_session_id": s.claude_id,
            "last_prompt": s.last_prompt,
            "diff": stats,
        })
    }

    fn control(&mut self, ctx: &egui::Context, request: ControlRequest, reply: Sender<ControlOutcome>) {
        let done = |response: ControlResponse| ControlOutcome::Done(response);
        let missing = |reference: &str| done(ControlResponse::error(format!("no session matches {reference:?}")));
        let outcome = match request {
            ControlRequest::List => {
                let sessions: Vec<_> = (0..self.sessions.len()).map(|i| self.describe(i)).collect();
                let (window, monitor) = ctx.input(|i| (i.content_rect().size(), i.viewport().monitor_size));
                let layout = self.last_layout.map(|l| {
                    json!({ "sidebar": l.sidebar, "diff": format!("{:?}", l.diff), "columns": l.columns, "rows": l.rows, "tiled": l.tiled, "tabs": l.tabs })
                });
                done(ControlResponse::ok(json!({
                    "active": self.active,
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
            ControlRequest::PlanSet { goal, tasks } => {
                self.plan.replace(goal, tasks);
                self.save_plan();
                self.panel = PanelMode::Plan;
                self.diff_open = true;
                let unknown = self.plan.unknown_dependencies();
                self.toast("New plan: review and approve it on the plan board");
                done(ControlResponse::ok(json!({
                    "tasks": self.plan.tasks.len(),
                    "approved": false,
                    "unknown_dependencies": unknown,
                    "next": "The user reviews and approves the plan on the board; then call plan_dispatch.",
                })))
            }
            ControlRequest::PlanGet => done(ControlResponse::ok(self.plan_json())),
            ControlRequest::PlanUpdate { id, status, note } => match self.plan.task_mut(&id) {
                None => done(ControlResponse::error(format!("no task {id}"))),
                Some(task) => {
                    if let Some(status) = &status {
                        match TaskStatus::parse(status) {
                            Some(status) => task.status = status,
                            None => {
                                let _ = reply.send(done(ControlResponse::error(format!("unknown status {status}"))));
                                return;
                            }
                        }
                    }
                    if let Some(note) = note {
                        task.notes.push(note);
                    }
                    self.save_plan();
                    done(ControlResponse::ok(self.plan_json()))
                }
            },
            ControlRequest::PlanDispatch { id } => match self.dispatch(id.as_deref()) {
                Ok(started) => done(ControlResponse::ok(json!({ "started": started }))),
                Err(err) => done(ControlResponse::error(err)),
            },
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
                Some(index) if raw => match &self.sessions[index].terminal {
                    Some(terminal) => {
                        terminal.write(unescape(&text).into_bytes());
                        done(ControlResponse::ok(json!({ "sent": true })))
                    }
                    None => done(ControlResponse::error("raw keys need a session running inside cox")),
                },
                Some(index) => {
                    if self.deliver(index, &text) {
                        let queued = self.sessions[index].terminal.is_none();
                        done(ControlResponse::ok(json!({ "sent": true, "queued_until_next_hook": queued })))
                    } else {
                        done(ControlResponse::error("session cannot receive messages (exited?)"))
                    }
                }
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
                    let state = self.diffs.entry(root).or_default();
                    let excerpt = state
                        .prepared
                        .as_ref()
                        .and_then(|p| p.diff.files.iter().find(|f| f.path() == path))
                        .and_then(|f| {
                            f.hunks.iter().flat_map(|h| &h.lines).find(|l| {
                                let number = if removed { l.old_no } else { l.new_no };
                                line.is_some()
                                    && number == line
                                    && (l.kind == cox_core::diff::LineKind::Removed) == removed
                            })
                        })
                        .map(|l| l.text.clone())
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
                        self.send_review(&root);
                        let left = self.diffs.get(&root).map(|d| d.comments.len()).unwrap_or(0);
                        if left == 0 {
                            done(ControlResponse::ok(json!({ "sent_comments": count })))
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

    fn toggle_plan(&mut self) {
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
        let mut child =
            ui.new_child(egui::UiBuilder::new().id_salt("plan-panel").max_rect(rect.shrink2(vec2(12.0, 8.0))));
        let ui = &mut child;
        let mut approve = false;
        let mut dispatch_all = false;
        let mut close = false;
        let ready = self.plan.ready().len();
        ui.horizontal(|ui| {
            ui.label(RichText::new("PLAN").color(p.accent).strong().size(13.0));
            let total = self.plan.tasks.len();
            if total > 0 {
                let done = self.plan.count(TaskStatus::Done);
                ui.label(RichText::new(format!("{done}/{total} done")).color(p.muted).size(12.0));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("×").on_hover_text("Back to the diff").clicked() {
                    close = true;
                }
                if total == 0 {
                    return;
                }
                if !self.plan.approved {
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
        if !self.plan.goal.is_empty() {
            ui.label(RichText::new(&self.plan.goal).color(p.foreground).size(13.0));
        }
        for (task, dep) in self.plan.unknown_dependencies() {
            ui.label(RichText::new(format!("{task} depends on unknown task {dep}")).color(p.error).size(11.5));
        }
        ui.add_space(6.0);
        if self.plan.tasks.is_empty() {
            ui.label(
                RichText::new(
                    "No plan yet. Ask an orchestrator (a Claude session with the cox MCP server) to plan a goal: \
                     its tasks appear here for you to review, edit and approve before any agent starts.",
                )
                .color(p.muted),
            );
        }

        let mut actions: Vec<(String, &str)> = Vec::new();
        let mut open_session: Option<String> = None;
        let editable = !self.plan.approved;
        let ready_ids: HashSet<String> = self.plan.ready().iter().map(|t| t.id.clone()).collect();
        let approved = self.plan.approved;
        let sessions: HashMap<String, (String, AgentStatus)> =
            self.sessions.iter().map(|s| (s.id.clone(), (s.title.clone(), s.status))).collect();
        let expanded = &mut self.expanded_tasks;
        let tasks = &mut self.plan.tasks;
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
                        .id_salt(("plan-column", status.label()))
                        .auto_shrink([false, false])
                        .show(column, |ui| {
                            for task in tasks.iter_mut().filter(|t| t.status == status) {
                                let open = expanded.contains(&task.id);
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
                    if !self.expanded_tasks.remove(&id) {
                        self.expanded_tasks.insert(id);
                    }
                }
                "edited" => changed = true,
                "start" => {
                    if let Err(err) = self.dispatch(Some(&id)) {
                        self.toast(err);
                    }
                }
                status => {
                    if let (Some(task), Some(status)) = (self.plan.task_mut(&id), TaskStatus::parse(status)) {
                        task.status = status;
                        changed = true;
                    }
                }
            }
        }
        if approve {
            self.plan.approved = true;
            changed = true;
            self.toast("Plan approved: the orchestrator can dispatch it now");
        }
        if dispatch_all && let Err(err) = self.dispatch(None) {
            self.toast(err);
        }
        if changed {
            self.save_plan();
        }
        if let Some(session) = open_session {
            self.activate(&session, true);
            self.panel = PanelMode::Diff;
        }
        if close {
            self.panel = PanelMode::Diff;
        }
    }

    fn save_plan(&self) {
        if let Err(err) = self.plan.save(&paths::plan_file()) {
            eprintln!("cox: saving the plan failed: {err:#}");
        }
    }

    fn plan_json(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(&self.plan).unwrap_or_default();
        value["ready"] = json!(self.plan.ready().iter().map(|t| &t.id).collect::<Vec<_>>());
        value
    }

    /// Starts `only`, or every ready task, each in a new worktree with its brief as
    /// the agent's first message.
    fn dispatch(&mut self, only: Option<&str>) -> Result<Vec<String>, String> {
        if !self.plan.approved {
            return Err("The plan is not approved yet: the user approves it on the plan board.".into());
        }
        let ready: Vec<Task> =
            self.plan.ready().into_iter().filter(|t| only.is_none_or(|id| t.id == id)).cloned().collect();
        if ready.is_empty() {
            return Err(match only {
                Some(id) => format!("Task {id} is not ready (it must be planned with all dependencies done)."),
                None => "No task is ready.".into(),
            });
        }
        let fallback_repo =
            self.diff_target().and_then(|t| self.project_of(&t)).or_else(|| self.repos.first().cloned());
        let mut started = Vec::new();
        for task in ready {
            let Some(repo) = task.repo.as_deref().map(paths::expand).or_else(|| fallback_repo.clone()) else {
                return Err("No project is open to branch from; give the task a repo.".into());
            };
            let branch = task.branch.clone().unwrap_or_else(|| format!("cox/{}-{}", task.id, short_slug(&task.title)));
            let agent = if task.agent.trim().is_empty() { "claude".to_string() } else { task.agent.clone() };
            let launch = Launch {
                agent,
                prompt: Some(task.compose_brief(&self.plan.goal)),
                args: task.args.clone(),
                reply: None,
                task: Some(task.id.clone()),
            };
            self.create_worktree(repo, branch.clone(), launch);
            if let Some(t) = self.plan.task_mut(&task.id) {
                t.status = TaskStatus::Running;
                t.branch = Some(branch);
            }
            started.push(task.id);
        }
        self.save_plan();
        Ok(started)
    }

    /// A sub-agent's `cox_report`: attach it to its task, move the task along, and
    /// wake anyone waiting.
    fn report(&mut self, pane: Option<&str>, status: &str, summary: &str) -> Option<String> {
        let session = pane.and_then(|p| self.sessions.iter().find(|s| s.id == p));
        let (session_id, title) =
            session.map(|s| (s.id.clone(), s.title.clone())).unwrap_or_else(|| ("".into(), "unknown agent".into()));
        let task_id =
            self.plan.tasks.iter().find(|t| t.session.as_deref() == Some(session_id.as_str())).map(|t| t.id.clone());
        if let Some(task) = task_id.as_deref().and_then(|id| self.plan.task_mut(id)) {
            task.report =
                Some(Report { status: status.to_string(), summary: summary.to_string(), at: cox_core::store::now() });
            match status {
                "done" => task.status = TaskStatus::Review,
                "blocked" | "question" => task.status = TaskStatus::Blocked,
                _ => {}
            }
        }
        self.save_plan();
        self.event_seq += 1;
        self.events.push(StatusEvent {
            seq: self.event_seq,
            session: session_id,
            title: title.clone(),
            status: session.map(|s| s.status).unwrap_or(AgentStatus::Idle),
            at: cox_core::store::now(),
            report: Some((status.to_string(), summary.to_string(), task_id.clone())),
        });
        self.serve_waiters();
        let what = task_id.as_deref().map(|t| format!("task {t}")).unwrap_or(title);
        self.toast(format!("{what} reported {status}"));
        task_id
    }

    /// Running tasks whose agent exited without reporting are blocked.
    fn sync_plan(&mut self) {
        let mut changed = false;
        for task in &mut self.plan.tasks {
            if task.status != TaskStatus::Running {
                continue;
            }
            let gone = match &task.session {
                Some(id) => self.sessions.iter().find(|s| &s.id == id).is_none_or(|s| s.status == AgentStatus::Exited),
                None => false,
            };
            if gone {
                task.status = TaskStatus::Blocked;
                task.notes.push("the agent exited before reporting".into());
                changed = true;
            }
        }
        if changed {
            self.save_plan();
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
                    title: session.title.clone(),
                    status: session.status,
                    at: cox_core::store::now(),
                    report: None,
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
            if terminal.exited() || session.status == AgentStatus::NeedsInput {
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
            eprintln!("cox: saving sessions failed: {err:#}");
        }
    }

    /// Sends text to a session: typed into cox's own terminals, queued for the
    /// next hook of sessions running elsewhere.
    fn deliver(&mut self, index: usize, text: &str) -> bool {
        let session = &mut self.sessions[index];
        if let Some(terminal) = &session.terminal {
            if terminal.exited() {
                return false;
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
            return true;
        }
        let Some(claude_id) = session.claude_id.clone() else { return false };
        self.hub.lock().unwrap().outbox.entry(claude_id).or_default().push(text.to_string());
        true
    }

    fn steer_targets(&self) -> Vec<usize> {
        let indices = 0..self.sessions.len();
        match self.steer_target {
            SteerTarget::Active => self.active_index().into_iter().collect::<Vec<usize>>(),
            SteerTarget::Agents => indices.filter(|&i| self.sessions[i].is_agent()).collect::<Vec<usize>>(),
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
        for i in targets {
            let external = self.sessions[i].terminal.is_none();
            if self.deliver(i, &text) {
                if external { queued += 1 } else { typed += 1 }
            }
        }
        self.steer_text.clear();
        let mut summary = format!("Sent to {typed} session{}", if typed == 1 { "" } else { "s" });
        if queued > 0 {
            summary.push_str(&format!(
                ", queued for {queued} outside cox (delivered at their next tool call or turn end)"
            ));
        }
        self.toast(summary);
    }

    fn send_review(&mut self, root: &Path) {
        let comments = self.diffs.get(root).map(|s| s.comments.clone()).unwrap_or_default();
        if comments.is_empty() {
            return;
        }
        let header = self.config.fill(&self.config.steer.review_header);
        let message = steer::compose_review(&header, &comments);
        let target = self
            .active_index()
            .filter(|&i| self.sessions[i].root() == root)
            .or_else(|| self.sessions.iter().position(|s| s.root() == root && s.is_agent() && s.can_receive()));
        let Some(index) = target else {
            self.toast("No agent is working in this worktree");
            return;
        };
        let count = comments.len();
        if self.deliver(index, &message) {
            if let Some(state) = self.diffs.get_mut(root) {
                state.comments.clear();
            }
            let title = self.sessions[index].title.clone();
            self.toast(format!("Sent {count} comment{} to {title}", if count == 1 { "" } else { "s" }));
        }
    }

    // ── background events ─────────────────────────────────────────────────

    fn pump(&mut self, ctx: &egui::Context) {
        for (agent, dir) in std::mem::take(&mut self.pending_open) {
            self.spawn(ctx, &agent, dir, None);
        }
        while let Ok(message) = self.hook_rx.try_recv() {
            self.on_hook(message);
        }
        while let Ok((branch, result, launch)) = self.worktree_rx.try_recv() {
            self.worktrees_at = None;
            if let Some(i) = self.creating.iter().position(|b| *b == branch) {
                self.creating.remove(i);
            }
            let outcome =
                result.and_then(|path| self.start(ctx, &launch.agent, path, None, launch.args, launch.prompt));
            if let Some(task_id) = &launch.task
                && let Some(task) = self.plan.task_mut(task_id)
            {
                match &outcome {
                    Ok(id) => task.session = Some(id.clone()),
                    Err(err) => {
                        task.status = TaskStatus::Blocked;
                        task.notes.push(format!("could not start: {err}"));
                    }
                }
                self.save_plan();
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
        while let Ok(message) = self.control_rx.try_recv() {
            self.control(ctx, message.request, message.reply);
        }
        self.track_busy();
        self.verify_deliveries();
        self.record_events();
        self.sync_plan();
        if !self.waiters.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        if self.sessions.iter().any(|s| s.status == AgentStatus::Working) {
            // Keeps the status pulse of working agents moving.
            ctx.request_repaint_after(Duration::from_millis(150));
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

        if let Some(root) = self.diff_target() {
            self.watch(ctx, &root);
            let state = self.diffs.entry(root).or_default();
            if state.prepared.is_none() && !state.loading && state.error.is_none() && state.dirty_since.is_none() {
                state.dirty_since = Some(Instant::now() - Duration::from_secs(1));
            }
        }
        // Agents off screen keep their sidebar counts current while they work.
        for session in &self.sessions {
            let state = self.diffs.entry(session.root()).or_default();
            let stale = state.last_request.is_none_or(|at| at.elapsed() > BACKGROUND_REFRESH);
            if (session.status == AgentStatus::Working || state.prepared.is_none()) && stale && !state.loading {
                state.mark_dirty();
            }
        }
        let mut waiting = false;
        for (root, state) in &mut self.diffs {
            if state.due() {
                self.worker.request(root.clone(), self.config.base_ref.clone(), self.highlighter.clone());
            } else if state.dirty_since.is_some() {
                waiting = true;
            }
        }
        if waiting {
            ctx.request_repaint_after(Duration::from_millis(120));
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
        let found =
            message.pane.as_ref().and_then(|pane| self.sessions.iter().position(|s| &s.id == pane)).or_else(|| {
                self.sessions.iter().position(|s| s.claude_id.as_deref() == Some(event.session_id.as_str()))
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
                    selected: false,
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
        if let Some(cwd) = &event.cwd
            && *cwd != session.cwd
        {
            session.cwd = cwd.clone();
            session.worktree = git::worktree_of(cwd);
            session.title = title_for(session.branch(), cwd);
        }
        if let Some(status) = event.status() {
            session.status = status;
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
    }

    fn watch(&mut self, ctx: &egui::Context, root: &Path) {
        if self.watcher.as_ref().is_some_and(|(r, _)| r == root) {
            return;
        }
        self.watcher = None;
        let (tx, ctx, watched) = (self.watch_tx.clone(), ctx.clone(), root.to_path_buf());
        let handler = move |result: DebounceEventResult| {
            let Ok(events) = result else { return };
            let relevant = events.iter().any(|e| {
                !e.path
                    .strip_prefix(&watched)
                    .unwrap_or(&e.path)
                    .components()
                    .any(|c| IGNORED_DIRS.iter().any(|d| c.as_os_str() == *d))
            });
            if relevant {
                let _ = tx.send(watched.clone());
                ctx.request_repaint();
            }
        };
        match new_debouncer(Duration::from_millis(250), handler) {
            Ok(mut debouncer) => {
                if debouncer.watcher().watch(root, RecursiveMode::Recursive).is_ok() {
                    self.watcher = Some((root.to_path_buf(), debouncer));
                }
            }
            Err(err) => eprintln!("cox: file watcher failed: {err}"),
        }
    }

    // ── input ─────────────────────────────────────────────────────────────

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let keys = self.config.keys.clone();
        let pressed = |text: &str| shortcut(text).is_some_and(|s| ctx.input_mut(|i| i.consume_shortcut(&s)));
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
            if ctx.input_mut(|i| i.consume_shortcut(&chord))
                && let Some(id) = self.sessions.get(n).map(|s| s.id.clone())
            {
                self.activate(&id, false);
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
                let (done, total) = (self.plan.count(TaskStatus::Done), self.plan.tasks.len());
                let label = if total == 0 { "Plan".to_string() } else { format!("Plan {done}/{total}") };
                let attention = self.plan.count(TaskStatus::Review) + self.plan.count(TaskStatus::Blocked);
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
        let total = ui.available_height();
        let sessions_height = if files_root.is_some() { (total * 0.46).max(150.0) } else { total };
        ui.add_space(6.0);
        egui::ScrollArea::vertical().id_salt("sidebar").max_height(sessions_height).auto_shrink([false, true]).show(ui, |ui| {
            section(ui, &p, "AGENTS");
            let mut clicked: Option<String> = None;
            let mut close: Option<String> = None;
            for session in &mut self.sessions {
                let active = self.active.as_deref() == Some(session.id.as_str());
                let stats = self.diffs.get(&session.root()).and_then(|d| d.prepared.as_ref()).map(|p| (p.diff.additions(), p.diff.deletions()));
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
                .filter(|r| !live_ids.contains(r.claude_session_id.as_str()) && r.cwd.is_dir())
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
                "Adds `cox hook` to ~/.claude/settings.json so agents in any terminal show up here and can be steered",
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
        ] {
            ui.label(RichText::new(format!("{key}  {what}")).size(12.0).monospace());
        }
    }

    fn pending_open_resume(&mut self, record: SessionRecord) {
        self.resume.push(record);
    }

    fn tab_strip(&mut self, ui: &mut egui::Ui, rect: Rect, tiled: &[usize]) {
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
                for (i, s) in self.sessions.iter().enumerate() {
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
            let body = tile.with_min_y(header.bottom());
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
                        RichText::new(format!("Running outside cox in {}", paths::display(&session.cwd)))
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
        let state = self.diffs.entry(root.clone()).or_default();
        let header = Header { title: &title, branch: branch.as_deref(), can_send };
        match diff_view::show(ui, rect, state, header, &palette, &font) {
            DiffAction::None => {}
            DiffAction::SendReview => self.send_review(&root),
            DiffAction::Refresh => state_refresh(self.diffs.entry(root).or_default()),
            DiffAction::Close => self.diff_open = false,
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
            [("TERM", "xterm-256color"), ("COLORTERM", "truecolor"), ("TERM_PROGRAM", "cox")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        if let Some(shell_path) = SHELL_PATH.as_ref() {
            env.insert("PATH".into(), shell_path.clone());
        }
        match Terminal::spawn(ctx, self.palette.clone(), resolve_program(program), args, root, env) {
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
            self.create_worktree(repo, branch, launch);
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
        let mut all = prepared.all_files.clone();
        all.extend(prepared.diff.files.iter().map(|f| f.path().to_string()));
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
                let matches = cox_core::files::best_matches(&dialog.query, &all, 14);
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
                    eprintln!("cox: screenshot failed: {err:#}");
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
        let mut layout = layout::compute(area.width(), area.height(), self.sessions.len(), diff_open, &layout_config);
        if layout.tabs {
            layout = layout::compute(
                area.width(),
                area.height() - TAB_HEIGHT,
                self.sessions.len(),
                diff_open,
                &layout_config,
            );
            layout.tabs = self.sessions.len() > layout.tiled || layout.tiled == 0;
        }

        self.last_layout = Some(layout);
        if layout.sidebar {
            egui::Panel::left("sidebar")
                .exact_size(self.config.layout.sidebar_width)
                .resizable(false)
                .frame(egui::Frame::new().fill(p.panel).inner_margin(egui::Margin::symmetric(10, 4)))
                .show(ui, |ui| self.sidebar(ui));
        }

        egui::CentralPanel::default().frame(egui::Frame::new().fill(p.background)).show(ui, |ui| {
            let mut rect = ui.max_rect();
            let statuses: Vec<AgentStatus> = self.sessions.iter().map(|s| s.status).collect();
            let tiled = pick_tiled(&statuses, self.active_index(), layout.tiled);
            if layout.tabs {
                self.tab_strip(ui, rect.with_max_y(rect.top() + TAB_HEIGHT), &tiled);
                rect.min.y += TAB_HEIGHT;
            }
            if self.sessions.is_empty() && !diff_open {
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
                if self.sessions.is_empty() {
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

fn resolve_program(program: &str) -> String {
    if program.contains('/') {
        return program.to_string();
    }
    let path = SHELL_PATH.clone().or_else(|| std::env::var("PATH").ok()).unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_string())
}

fn open_path(path: &Path) -> std::io::Result<std::process::Child> {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    std::process::Command::new(opener).arg(path).spawn()
}

fn claude_settings_path() -> PathBuf {
    paths::home().join(".claude").join("settings.json")
}

fn global_hooks_installed() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    std::fs::read_to_string(claude_settings_path()).is_ok_and(|text| text.contains(&format!("{} hook", exe.display())))
}

pub fn install_global_hooks() -> anyhow::Result<PathBuf> {
    let path = claude_settings_path();
    let exe = std::env::current_exe()?;
    let mut settings: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(text) => {
            std::fs::write(path.with_extension("json.cox-backup"), &text)?;
            serde_json::from_str(&text)?
        }
        Err(_) => serde_json::json!({}),
    };
    cox_core::hook::merge_into_settings(&mut settings, &exe);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&settings)?)?;
    Ok(path)
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

/// A short branch-friendly version of a task title.
fn short_slug(title: &str) -> String {
    let lower: String = title.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let mut slug = lower.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    slug.truncate(40);
    slug.trim_end_matches('-').to_string()
}
