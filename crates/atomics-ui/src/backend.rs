use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use atomics_core::approvals::ApprovalRequest;
use atomics_core::config::{AgentKind, ChiefDock, FontConfig, KeyConfig};
use atomics_core::diff_data::{FileView, Prepared};
use atomics_core::layout::LayoutConfig;
use atomics_core::plan::{Plan, PlanStatus, Task};
pub use atomics_core::projection::TaskLink;
use atomics_core::repo_workflow::RepoWorkflow;
use atomics_core::session::AgentStatus;
use atomics_core::steer::ReviewComment;
use atomics_core::store::SessionRecord;
use atomics_core::tickets::{TicketConfig, TicketLink};
use egui::Color32;

use crate::Command;
use crate::theme::Palette;

/// What a plan is doing, for its row in the Boards dropdown. Selection is independent of activity.
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

    /// The color a plan's goal and name are drawn in.
    pub fn color(self, palette: &Palette) -> Color32 {
        match self {
            Self::Running(_) => palette.accent,
            Self::NeedsYou => palette.warning,
            Self::Idle => palette.foreground,
            Self::Completed | Self::Archived => palette.muted,
        }
    }
}

/// One plan as the Boards dropdown and the top bar show it.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanRow {
    pub id: String,
    pub goal: String,
    /// Its short name: the plan's ticket with its progress, else the goal.
    pub tab_name: String,
    pub status: PlanStatus,
    pub state: PlanState,
    /// The user approved its tasks.
    pub approved: bool,
    /// Changed since the user last looked.
    pub changed: bool,
    pub done: usize,
    pub total: usize,
    /// Tasks in review or blocked.
    pub attention: usize,
}

impl PlanRow {
    /// Open, with tasks that wait for the user to approve them.
    pub fn awaits_approval(&self) -> bool {
        self.status == PlanStatus::Open && self.total > 0 && !self.approved
    }

    /// Its board waits for the user: to approve the plan, or to answer one of its tasks or agents.
    pub fn needs_user(&self) -> bool {
        self.awaits_approval() || self.state == PlanState::NeedsYou
    }
}

/// One session as the sidebar, the tab strip and the tile headers show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    /// A visible explanation when the session is waiting for approval, sign-in or setup.
    pub status_hint: Option<String>,
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
    /// The plan and the task it works on, by id, if it runs a plan's task.
    pub task: Option<TaskLink>,
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

impl SessionRow {
    pub fn status_label(&self) -> &str {
        self.status_hint.as_deref().unwrap_or(self.status.label())
    }

    /// A completed turn can be reviewed later; permissions and questions need an answer now.
    pub fn needs_user(&self) -> bool {
        self.status == AgentStatus::NeedsInput || self.pending_approvals > 0
    }

    pub fn hover(&self) -> String {
        let branch = self.branch.as_ref().map(|branch| format!("\n{branch}")).unwrap_or_default();
        format!("{} · {}{branch}", self.title, self.status_label())
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;

    #[test]
    fn finished_turns_do_not_need_the_user_and_hints_explain_input() {
        let mut row = SessionRow {
            id: "worker".into(),
            title: "t1 · Fix the parser".into(),
            agent: "agent".into(),
            kind: AgentKind::Generic,
            status: AgentStatus::Done,
            status_hint: None,
            chief: false,
            worktree: None,
            branch: Some("atomics/parse/t1-fix-parser".into()),
            root: PathBuf::from("/work"),
            project: None,
            ungrouped: false,
            task: None,
            scratch: false,
            sandbox: None,
            outside: false,
            can_receive: true,
            pending_approvals: 0,
            held_messages: 0,
            changes: None,
        };
        assert!(!row.needs_user());
        assert_eq!(row.status_label(), "turn done");
        row.pending_approvals = 1;
        assert!(row.needs_user());
        row.pending_approvals = 0;
        row.status = AgentStatus::NeedsInput;
        row.status_hint = Some("needs sign-in".into());
        assert!(row.needs_user());
        assert_eq!(row.status_label(), "needs sign-in");
        assert_eq!(row.hover(), "t1 · Fix the parser · needs sign-in\natomics/parse/t1-fix-parser");
    }
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
    /// How its agent runs that the user should know of, such as Codex started without ATOMICS's hooks.
    pub warning: Option<String>,
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

/// The host's Tickets panel, as the view switcher shows it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TicketsPanel {
    /// The ticket system wants the user to sign in.
    pub signed_out: bool,
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

#[derive(Debug, Clone, Default)]
pub struct FolderSnapshot {
    pub entries: Vec<atomics_core::folder::Entry>,
    pub loading: bool,
    pub more: bool,
    pub first: bool,
    pub error: Option<String>,
}

/// Everything the UI reads from a host, and the way its commands reach the host. Methods take `&self` and
/// return owned or shared data, so an implementation can sit over a lock, a snapshot or a socket.
pub trait Backend {
    fn editors(&self) -> Vec<atomics_core::projection::EditorInfo> {
        Vec::new()
    }

    fn pull_requests(&self) -> Vec<atomics_core::projection::PullRequestInfo> {
        Vec::new()
    }
    /// Carries out a host's command: at once, or queued to run in order with the host's other work. What
    /// becomes of it shows in later reads. A view command is the UI's own and never comes here.
    fn send(&self, command: Command);

    /// Whether this UI can carry out `command` now: a host's command, through [`send`](Backend::send), or a
    /// view command, on the platform. The views draw no button, menu entry or tab for a command it cannot,
    /// so what a client cannot do is absent rather than refused. Only the command's kind and the host it
    /// names count, not its other fields.
    fn available(&self, command: &Command) -> bool;

    /// Whether this backend can tell agents from shells. Only then can a message go to every agent: typed into a
    /// shell, it would run as a command.
    fn tells_agents_from_shells(&self) -> bool {
        true
    }

    /// Whether the platform takes the `[keys]` chords, so that a tooltip may name them.
    fn has_shortcuts(&self) -> bool {
        true
    }

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

    fn machine_available(&self) -> bool {
        false
    }
    fn machine(&self) -> Option<atomics_core::machine::Machine> {
        None
    }
    /// Declare this UI's interest, without filesystem or subprocess work on the draw thread.
    fn watch_machine(&self, _open: bool) {}

    /// Preview descriptions are shared by tiles, details and machine views.
    fn previews(&self, _session: Option<&str>) -> Vec<atomics_core::preview::Preview> {
        Vec::new()
    }
    fn preview_active(&self, _port: u16) -> bool {
        false
    }
    fn can_preview(&self) -> bool {
        true
    }

    /// The ticket a session works on, for the reader to follow: its task's or that task's plan's, among the open
    /// plans, else the selected plan's. It names the ticket system.
    fn session_ticket(&self, session: &str) -> Option<TicketLink> {
        let open = self.plan_list().into_iter().enumerate().filter(|(_, row)| row.status == PlanStatus::Open);
        let of_task = open.filter_map(|(index, _)| self.board(index)).find_map(|board| {
            let task = board.plan.tasks.iter().find(|task| task.session.as_deref() == Some(session))?;
            Some(task.ticket.clone().or_else(|| board.plan.ticket.clone()))
        });
        let mut link =
            of_task.flatten().or_else(|| self.board(self.view_hints().selected_plan).and_then(|b| b.plan.ticket))?;
        if link.system.is_empty()
            && let Some(info) = self.tickets()
        {
            link.system = info.system;
        }
        Some(link)
    }

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

    /// Unix milliseconds from the platform's clock, for running trace durations.
    fn trace_now_ms(&self) -> u64 {
        0
    }

    /// The current chief's cleaned, bounded calls.
    fn trace(&self) -> atomics_core::trace_feed::TraceData {
        Default::default()
    }

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

    /// Remote file trees arrive a folder at a time. Native backends may keep the prepared tree.
    fn paged_folders(&self) -> bool {
        false
    }
    fn folder(&self, _root: &Path, _path: &str) -> FolderSnapshot {
        FolderSnapshot::default()
    }
    fn more_folder(&self, _root: &Path, _path: &str) {}
    fn first_folder(&self, _root: &Path, _path: &str) {}

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

    /// Host management data, already scrubbed of configuration credentials.
    fn management(&self) -> Option<atomics_core::projection::ManagementInfo> {
        None
    }

    /// The project the host was started in, where the welcome starts agents outside the Ungrouped tab;
    /// `None` when the host does not say.
    fn launch_dir(&self) -> Option<PathBuf>;

    /// The selected plan's safe ticket links, including board tickets without a task.
    fn ticket_tree(&self, _plan: usize) -> Option<atomics_core::ticket_ui::TicketTree> {
        None
    }

    /// The shared reader's safe profile and bounded request replies; absent on older hosts.
    fn tickets(&self) -> Option<atomics_core::ticket_ui::TicketsInfo> {
        None
    }

    fn tickets_panel(&self) -> Option<TicketsPanel> {
        None
    }

    /// The ticket system's sign-in, when the host's ticket system signs in with OAuth and says how it stands.
    fn sign_in(&self) -> Option<crate::signin::SignInView> {
        None
    }

    /// The host's account usage and load, when it publishes them.
    fn resources(&self) -> Option<atomics_core::projection::ResourceInfo> {
        None
    }

    /// The host's version and any newer one, when it publishes them.
    fn update(&self) -> Option<atomics_core::projection::UpdateInfo> {
        None
    }
}
