use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use atomics_core::approvals::ApprovalRequest;
use atomics_core::config::{AgentKind, ChiefDock, FontConfig, KeyConfig};
use atomics_core::diff_data::{FileView, Prepared};
use atomics_core::layout::LayoutConfig;
use atomics_core::plan::{Plan, PlanStatus, Task};
use atomics_core::repo_workflow::RepoWorkflow;
use atomics_core::session::AgentStatus;
use atomics_core::steer::ReviewComment;
use atomics_core::store::SessionRecord;
use atomics_core::tickets::TicketConfig;
use egui::Color32;

use crate::Command;
use crate::theme::Palette;

/// What a plan is doing, for its tab. Selection is independent of activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanState {
    Running(usize),
    NeedsYou,
    Idle,
    Completed,
    Archived,
}

impl PlanState {
    pub fn urgency(self) -> u8 {
        match self {
            Self::NeedsYou => 2,
            Self::Running(_) => 1,
            _ => 0,
        }
    }

    pub fn label(self, goal: &str) -> String {
        let name: String = goal.chars().take(28).collect();
        match self {
            Self::Running(count) => format!("● {name} · {count} running"),
            Self::NeedsYou => format!("◆ {name} · needs you"),
            Self::Idle => format!("{name} · idle"),
            Self::Completed => format!("✓ {name}"),
            Self::Archived => format!("✓ {name} · archived"),
        }
    }

    /// The color a plan's goal and tab are drawn in.
    pub fn color(self, palette: &Palette) -> Color32 {
        match self {
            Self::Running(_) => palette.accent,
            Self::NeedsYou => palette.warning,
            Self::Idle => palette.foreground,
            Self::Completed | Self::Archived => palette.muted,
        }
    }
}

/// One plan as the tabs and the top bar show it.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanRow {
    pub id: String,
    pub goal: String,
    /// The tab's name: the plan's ticket with its progress, else the goal.
    pub tab_name: String,
    pub status: PlanStatus,
    pub state: PlanState,
    /// Changed since the user last looked.
    pub changed: bool,
    pub done: usize,
    pub total: usize,
    /// Tasks in review or blocked.
    pub attention: usize,
}

/// One session as the sidebar, the tab strip and the tile headers show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub id: String,
    pub title: String,
    /// The configured agent it runs, by name.
    pub agent: String,
    pub kind: AgentKind,
    pub status: AgentStatus,
    /// The chief, which stays outside the agent fleet.
    pub chief: bool,
    /// The worktree it runs in, if any.
    pub worktree: Option<PathBuf>,
    /// Its worktree's branch; `None` outside a worktree or on a detached HEAD.
    pub branch: Option<String>,
    /// Its worktree, else its folder: where its diff is read.
    pub root: PathBuf,
    /// The open project `root` belongs to, if any.
    pub project: Option<PathBuf>,
    /// Neither the chief nor in any plan's task: the Ungrouped tab shows it.
    pub ungrouped: bool,
    /// It works in a scratch folder of its own, outside any project.
    pub scratch: bool,
    /// The sandbox it runs in, if any.
    pub sandbox: Option<String>,
    /// It runs in a terminal outside ATOMICS, which sees it through its hooks.
    pub outside: bool,
    /// It can take a message now: its terminal runs, or, outside ATOMICS, its conversation is known and has
    /// not exited.
    pub can_receive: bool,
    /// Permission requests waiting for a decision.
    pub pending_approvals: usize,
    /// Messages held back until those requests are decided.
    pub held_messages: usize,
    /// Lines added and removed in the diff of `root`, once the host has read it.
    pub changes: Option<(usize, usize)>,
}

/// What a session's tile shows beyond its [`SessionRow`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TileView {
    /// What the session's plan task last reported, and how long ago, while the session is idle or done.
    pub reported: Option<String>,
    /// The memory and CPU of the session's sandbox, once the host has sampled it.
    pub sandbox_usage: Option<String>,
    /// Why the host's last sample of the sandboxes failed.
    pub sandbox_error: Option<String>,
    /// What the host's slot queue holds for the session's processes, e.g. "queued for a test slot".
    pub queue_note: Option<String>,
    /// The MCP servers the sandboxed session needs and lacks, as its banner offers to connect them.
    pub connect: Vec<ConnectButton>,
    /// Its process exited; the tile stays until it is closed.
    pub exited: bool,
    /// Once exited, its agent can pick its conversation up again: a Claude session with a saved id.
    pub resumable: bool,
    /// Its folder, as the host writes paths: a session outside ATOMICS says where it runs.
    pub folder: String,
    /// What the user last asked a session outside ATOMICS, and its last answer.
    pub last_prompt: Option<String>,
    pub last_message: Option<String>,
}

/// A Connect button of a tile's banner, for an MCP server the session needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectButton {
    /// The server.
    pub name: String,
    pub label: String,
    /// What connecting it would use, and who asked.
    pub detail: String,
    /// The URL the agent named for it, if any.
    pub url: Option<String>,
    /// Connecting stores its environment first.
    pub store_env: bool,
    pub enabled: bool,
}

/// A finished plan task whose session is gone, as its tile shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletedTile {
    /// Its plan, by position in [`Backend::plan_list`].
    pub plan: usize,
    /// Its position in the plan.
    pub index: usize,
    pub task: Task,
    /// Its plan is no longer open, so the task cannot be resumed.
    pub read_only: bool,
    /// Its worktree is still on the host: its `diff` and `resume` need it.
    pub worktree: bool,
    /// It ran a Codex agent, which resumes a session without a saved id.
    pub codex: bool,
    /// The host's clock when it read the task, in seconds since the epoch.
    pub now: u64,
}

/// One plan as its board shows it: the plan header and the kanban columns. The plan comes whole; the
/// other fields are what the board reads beside it from the rest of the host's model.
#[derive(Debug, Clone, PartialEq)]
pub struct BoardView {
    /// The plan as the host holds it: goal, status, tasks, ticket and stages.
    pub plan: Plan,
    /// What the plan is doing; it colors the goal.
    pub state: PlanState,
    /// Planned tasks whose dependencies are done and integrated, by id, in plan order.
    pub ready: Vec<String>,
    /// Every session's title and status, by session id: a card's session line and its open button.
    pub sessions: HashMap<String, (String, AgentStatus)>,
    /// Why a task's integration is not verified yet, by task id, and whether a check of it runs now.
    pub integration_notes: HashMap<String, (String, bool)>,
    /// An integration freshness check of this plan runs.
    pub checking: bool,
    /// The plan's tasks being integrated now, by id.
    pub integrating: HashSet<String>,
    /// The `[tickets]` in effect, once the host has read it.
    pub ticket_config: Option<Arc<TicketConfig>>,
    /// The tickets on the plan's board, and how many of them no task follows, once the host has read them.
    pub board_counts: Option<(usize, usize)>,
    /// Finished tasks whose worktree is still on the host, by id: their `diff` and `resume` need it.
    pub worktrees: HashSet<String>,
    /// Tasks run by a Codex agent, by id: Codex resumes a session without a saved id.
    pub codex: HashSet<String>,
    /// The host's clock when it read the board, in seconds since the epoch: how old a report or a ticket
    /// read is.
    pub now: u64,
}

/// The chief as its pill in the top bar and its window show it.
#[derive(Debug, Clone, PartialEq)]
pub struct ChiefView {
    /// The chief's session, by position in [`Backend::session_list`], while there is a chief.
    pub session: Option<usize>,
    /// Its window shows.
    pub open: bool,
    /// Docked beside the agents, or floating over them.
    pub dock: ChiefDock,
    /// The width the user gave the docked chief; `None` for the default.
    pub width: Option<f32>,
    /// The host's last heartbeat check, once one ran.
    pub heartbeat: Option<Heartbeat>,
}

/// The host's last heartbeat check, as the chief's pill shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heartbeat {
    /// Seconds since it ran, by the host's clock when it was read.
    pub age: u64,
    /// When it ran, as the host's local date and time.
    pub time: String,
    /// How many findings it reported.
    pub findings: usize,
}

/// A permission request a session waits on, as its approval banner shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRow {
    /// The session that asked, by id.
    pub session: String,
    /// The request's id. A decision names it, so the host refuses one made for an earlier request.
    pub id: String,
    pub request: ApprovalRequest,
}

/// A worktree's diff as the host last published it, and the whole file open beside it.
#[derive(Clone, Default)]
pub struct DiffSnapshot {
    /// The diff's revision, which a new review comment is anchored to.
    pub revision: u64,
    pub prepared: Option<Arc<Prepared>>,
    pub error: Option<String>,
    pub loading: bool,
    pub comments: Vec<ReviewComment>,
    /// The file the host was asked to load whole, if any.
    pub viewing: Option<String>,
    pub file: Option<Arc<FileView>>,
    pub file_error: Option<String>,
}

/// What the diff panel's header shows of a worktree beyond its diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHeading {
    /// The active session's title, else the worktree's folder name.
    pub title: String,
    /// The active session's branch, else the worktree's; `None` when it is the title.
    pub branch: Option<String>,
    /// An agent working there can take a review now.
    pub can_send: bool,
}

/// The parts of the host's config the UI is drawn by.
#[derive(Debug, Clone)]
pub struct UiConfig {
    pub keys: KeyConfig,
    /// Terminal and diff text sizes; a theme's sizes win over them.
    pub font: FontConfig,
    pub layout: LayoutConfig,
    /// The configured agents, by name, in the config's order.
    pub agents: Vec<String>,
}

/// What the diff panel shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelMode {
    Diff,
    Editor,
    /// The orchestrator's plan board.
    Plan,
    /// Find and read tickets from the configured ticket system.
    Tickets,
}

/// What the host's model holds about what is on screen. The desktop writes these view hints into the
/// model so that `atomics ctl` and the host's scheduling see them; the UI reads them back from there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewHints {
    /// The active session, by id.
    pub active: Option<String>,
    /// The worktree whose diff the diff panel shows: the active session's, else one picked in the explorer.
    pub diff_target: Option<PathBuf>,
    /// A plan's tab is on screen.
    pub plan_view: bool,
    /// The Ungrouped tab is on screen: every session outside a plan.
    pub ungrouped: bool,
    /// The project tab on screen; `None` shows every project.
    pub project: Option<PathBuf>,
    /// The plan of the plan tab and the board, by position in [`Backend::plan_list`].
    pub selected_plan: usize,
    pub diff_open: bool,
    pub panel: PanelMode,
}

/// The host's messages for the notice line, beyond the sessions that need the user.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Notices {
    /// The last short message, while it shows.
    pub toast: Option<String>,
    /// Why the config no longer loads; the last good one stays in effect.
    pub config_error: Option<String>,
    /// What the loaded config asks for that cannot work.
    pub config_problems: Option<String>,
    /// What the theme could not apply.
    pub theme_error: Option<String>,
    /// Why agents' hooks cannot reach the host.
    pub hook_error: Option<String>,
}

/// What the sandbox VMs' process trees use on the host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxLoad {
    /// Memory and CPU of all of them together, once sampled.
    pub total: Option<String>,
    /// How many VMs were sampled.
    pub sampled: usize,
    /// Why the last sample failed.
    pub error: Option<String>,
}

/// What the Project menu offers besides opening a folder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectMenu {
    /// Recently opened projects that are not open and still exist, with each as the host writes paths.
    pub recent: Vec<(PathBuf, String)>,
    /// The saved workspaces, by name.
    pub workspaces: Vec<String>,
    /// The workspace open now.
    pub current: Option<String>,
    /// Projects are open, so they can be saved as a workspace.
    pub can_save: bool,
}

/// A session that is no longer running, which the RECENT list offers to resume.
#[derive(Debug, Clone, PartialEq)]
pub struct RecentSession {
    pub record: SessionRecord,
    /// Its folder as the host writes paths, with the home folder as `~`.
    pub folder: String,
}

/// A worktree of a project, beside its main checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRow {
    pub path: PathBuf,
    /// Its branch; `None` on a detached HEAD.
    pub branch: Option<String>,
}

/// Everything the UI reads from a host, and the way its commands reach the host. Methods take `&self` and
/// return owned or shared data, so an implementation can sit over a lock, a snapshot or a socket.
pub trait Backend {
    /// Carries out a host's command: at once, or queued to run in order with the host's other work. What
    /// becomes of it shows in later reads. A view command is the UI's own and never comes here.
    fn send(&self, command: Command);

    /// Every plan, in the host's order. The UI addresses a plan by its position in this list.
    fn plan_list(&self) -> Vec<PlanRow>;

    /// One plan of [`plan_list`](Backend::plan_list), without building the others.
    fn plan_row(&self, index: usize) -> Option<PlanRow>;

    /// The board of the plan at `index` of [`plan_list`](Backend::plan_list).
    fn board(&self, index: usize) -> Option<BoardView>;

    /// The digest of the approval scope of the plan at `index` of [`plan_list`](Backend::plan_list), which
    /// approving it names.
    fn plan_digest(&self, index: usize) -> Option<String>;

    /// Every session, in the host's order. The UI addresses a session by its position in this list.
    fn session_list(&self) -> Vec<SessionRow>;

    /// One session of [`session_list`](Backend::session_list), without building the others.
    fn session_row(&self, index: usize) -> Option<SessionRow>;

    /// The first permission request the session at `index` of [`session_list`](Backend::session_list)
    /// waits on: the one its banner shows. Later ones wait behind it.
    fn session_approval(&self, index: usize) -> Option<ApprovalRow>;

    /// What the tile of the session at `index` of [`session_list`](Backend::session_list) shows beyond its
    /// row.
    fn tile(&self, index: usize) -> Option<TileView>;

    /// The finished plan tasks the current tab shows whose session is gone, in plan order: the fleet gives
    /// them the cells its sessions leave free.
    fn completed_tasks(&self) -> Vec<CompletedTile>;

    /// The worktrees the host has a diff for.
    fn diff_roots(&self) -> Vec<PathBuf>;

    /// The diff of one worktree.
    fn diff(&self, root: &Path) -> Option<DiffSnapshot>;

    /// The chief: its session, its window and the last heartbeat.
    fn chief(&self) -> ChiefView;

    /// What the diff panel's header shows of the worktree at `root`.
    fn diff_heading(&self, root: &Path) -> DiffHeading;

    /// The keys, fonts, layout and agents the host's config sets.
    fn ui_config(&self) -> UiConfig;

    /// The view hints in the host's model.
    fn view_hints(&self) -> ViewHints;

    /// The sessions the current tab shows, as positions in [`session_list`](Backend::session_list).
    fn visible_sessions(&self) -> Vec<usize>;

    /// The branches of worktrees being checked out for new sessions.
    fn checkouts(&self) -> Vec<String>;

    /// Up to eight sessions of the open projects that no longer run, newest first, which can be resumed.
    fn recent_sessions(&self) -> Vec<RecentSession>;

    /// The main checkouts of the open projects (and the config's `repos`).
    fn repos(&self) -> Vec<PathBuf>;

    /// The open project `dir` belongs to, if any.
    fn project_of(&self, dir: &Path) -> Option<PathBuf>;

    /// The repository workflow of a project: its instructions, config and warnings, once read.
    fn repo_workflow(&self, project: &Path) -> Option<RepoWorkflow>;

    /// A project's worktrees other than its main checkout, as the host last listed them.
    fn worktrees(&self, project: &Path) -> Vec<WorktreeRow>;

    /// `path` as the host writes paths, with its home folder as `~`.
    fn display(&self, path: &Path) -> String;

    /// When the plan at `index` of [`plan_list`](Backend::plan_list) was last saved, or its last task
    /// finished, as the host's local date.
    fn plan_date(&self, index: usize) -> String;

    /// The notice line's messages.
    fn notices(&self) -> Notices;

    /// What the sandbox VMs use on the host.
    fn sandbox_load(&self) -> SandboxLoad;

    /// The Project menu's recent projects and workspaces.
    fn project_menu(&self) -> ProjectMenu;
}
