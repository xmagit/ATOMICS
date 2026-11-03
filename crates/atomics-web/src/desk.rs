//! What a page shows of its host, kept as the desktop keeps it in its model: the active session, the tab, the
//! right panel and the chief's window. The desktop writes these into the host's model (`Desktop::send`); a
//! browser keeps its own, since they are this page's and not the host's, and gives them to the views as the
//! [`ViewHints`] and the [`ChiefView`] the [`RemoteBackend`](crate::remote::RemoteBackend) answers with.
//! [`Desk::apply`] carries out the view commands the way the desktop does.

use std::path::PathBuf;

use atomics_core::config::ChiefDock;
use atomics_ui::{Backend, Command, PanelMode, ViewHints};

/// The chief's window in this page: shown or not, docked or floating, and the width the user dragged.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChiefWindow {
    pub open: bool,
    pub dock: ChiefDock,
    pub width: Option<f32>,
}

/// One page's view of its host.
#[derive(Debug, Clone, PartialEq)]
pub struct Desk {
    /// The active session, by id.
    pub active: Option<String>,
    /// A worktree picked without a session (a task's diff, the diffs list): the diff's target while no
    /// session is active.
    pub diff_root: Option<PathBuf>,
    pub diff_open: bool,
    pub panel: PanelMode,
    /// A plan's tab is on screen.
    pub plan_view: bool,
    /// The Ungrouped tab is on screen.
    pub ungrouped: bool,
    /// The plan of the plan tab and the board, by position in the plan list.
    pub selected_plan: usize,
    pub chief: ChiefWindow,
    /// The terminal last clicked into while it is not the active session's (the chief's), by id. The
    /// terminal input goes there, else to the active session's.
    pub typing_in: Option<String>,
    /// A tab opened: the next [`Desk::settle`] makes its first session the active one unless the tab shows
    /// the active session.
    pick: bool,
}

impl Default for Desk {
    /// The first plan's tab with its board beside the agents, and the chief docked.
    fn default() -> Self {
        Desk {
            active: None,
            diff_root: None,
            diff_open: true,
            panel: PanelMode::Plan,
            plan_view: true,
            ungrouped: false,
            selected_plan: 0,
            chief: ChiefWindow { open: true, dock: ChiefDock::Docked, width: None },
            typing_in: None,
            pick: true,
        }
    }
}

/// The commands about the chief's window, which the desktop sends its host (they start a chief, or are saved
/// in its store) and a page keeps to itself.
pub fn window_command(command: &Command) -> bool {
    matches!(
        command,
        Command::ToggleChief | Command::SetChiefDock(_) | Command::SaveChiefPreferences | Command::HideChief
    ) || matches!(command, Command::ResizeChief(_))
}

/// Whether a page carries out `command` itself: the view commands it can follow, and in the wide layout
/// (`wide`) the chief's window. The views draw no action for the others.
pub fn carries_out(command: &Command, wide: bool) -> bool {
    match command {
        Command::Activate { .. }
        | Command::OpenTaskSession(_)
        | Command::ShowTaskDiff { .. }
        | Command::ShowRoot(_)
        | Command::OpenDiff
        | Command::ToggleDiff
        | Command::CloseDiff
        | Command::SetPanel(PanelMode::Diff | PanelMode::Plan)
        | Command::TogglePlan
        | Command::ShowPlan(_)
        | Command::ShowUngrouped
        | Command::ViewPlanHistory(_)
        | Command::EditingPlan
        | Command::Toast(_) => true,
        command if window_command(command) => wide,
        _ => false,
    }
}

impl Desk {
    /// The worktree whose diff the panel shows: the active session's, else the one picked.
    pub fn diff_target(&self, backend: &dyn Backend) -> Option<PathBuf> {
        match self.active.as_deref().and_then(|id| backend.session_list().into_iter().find(|s| s.id == id)) {
            Some(session) => Some(session.root),
            None => self.diff_root.clone(),
        }
    }

    /// What the views read back as the host's view hints.
    pub fn hints(&self, backend: &dyn Backend) -> ViewHints {
        ViewHints {
            active: self.active.clone(),
            diff_target: self.diff_target(backend),
            plan_view: self.plan_view,
            ungrouped: self.ungrouped,
            project: None,
            selected_plan: self.selected_plan,
            diff_open: self.diff_open,
            panel: self.panel,
        }
    }

    /// The terminal the page's input goes to: the one clicked into, else the active session's.
    pub fn typing_target(&self) -> Option<String> {
        self.typing_in.clone().or_else(|| self.active.clone())
    }

    /// Keeps what this page shows to what the host has: a plan that exists, a plan tab only while there is a
    /// plan, and an active session that runs, else the first the tab shows (as the desktop picks one when a
    /// tab opens). `visible` reads the tab's sessions, as positions in the session list, for these hints.
    pub fn settle(&mut self, backend: &dyn Backend, visible: impl Fn(&ViewHints) -> Vec<usize>) {
        let plans = backend.plan_list().len();
        self.selected_plan = self.selected_plan.min(plans.saturating_sub(1));
        if plans == 0 {
            self.plan_view = false;
        }
        let sessions = backend.session_list();
        if self.typing_in.as_deref().is_some_and(|id| !sessions.iter().any(|s| s.id == id)) {
            self.typing_in = None;
        }
        let shown = visible(&self.hints(backend));
        let position = self.active.as_deref().and_then(|id| sessions.iter().position(|s| s.id == id && !s.chief));
        let pick = std::mem::take(&mut self.pick);
        // No session active stays so (a worktree picked on its own), unless a tab just opened.
        let keep = match position {
            Some(at) => !pick || shown.contains(&at),
            None => self.active.is_none() && !pick,
        };
        if !keep {
            self.active = shown.first().map(|&i| sessions[i].id.clone());
        }
        // A tab with no session yet picks its first once one comes.
        self.pick = pick && shown.is_empty();
    }

    /// Carries out a view command, or one about the chief's window, as the desktop does.
    pub fn apply(&mut self, command: Command, backend: &dyn Backend) {
        let root = |id: &str| backend.session_list().into_iter().find(|s| s.id == id).map(|s| s.root);
        match command {
            Command::Activate { session, open_diff } => self.activate(session, open_diff),
            Command::OpenTaskSession(session) => {
                self.activate(session, true);
                self.panel = PanelMode::Diff;
            }
            Command::ShowTaskDiff { plan, task } => {
                let session = backend.board(plan).and_then(|b| b.plan.tasks.get(task).and_then(|t| t.session.clone()));
                if let Some(worktree) = session.as_deref().and_then(root) {
                    self.show_root(worktree);
                    self.panel = PanelMode::Diff;
                }
            }
            Command::ShowRoot(worktree) => self.show_root(worktree),
            Command::OpenDiff => self.diff_open = true,
            Command::CloseDiff => self.diff_open = false,
            Command::ToggleDiff => self.diff_open = !self.diff_open,
            Command::SetPanel(panel) => self.panel = panel,
            Command::TogglePlan => {
                if self.panel == PanelMode::Plan && self.diff_open {
                    self.panel = PanelMode::Diff;
                } else {
                    self.panel = PanelMode::Plan;
                    self.diff_open = true;
                }
            }
            Command::ShowPlan(plan) => {
                self.selected_plan = plan;
                self.plan_view = true;
                self.ungrouped = false;
                self.panel = PanelMode::Diff;
                self.pick = true;
            }
            Command::ViewPlanHistory(plan) => {
                self.selected_plan = plan;
                self.plan_view = true;
                self.ungrouped = false;
                self.panel = PanelMode::Plan;
            }
            Command::ShowUngrouped => {
                self.plan_view = false;
                self.ungrouped = true;
                self.pick = true;
            }
            Command::ToggleChief => self.chief.open = !self.chief.open,
            Command::HideChief => self.chief.open = false,
            Command::SetChiefDock(dock) => self.chief.dock = dock,
            Command::ResizeChief(width) => self.chief.width = width,
            _ => {}
        }
        if !self.chief.open {
            self.typing_in = None;
        }
    }

    fn activate(&mut self, session: String, open_diff: bool) {
        self.active = Some(session);
        self.diff_root = None;
        self.typing_in = None;
        if open_diff {
            self.diff_open = true;
        }
    }

    fn show_root(&mut self, worktree: PathBuf) {
        self.active = None;
        self.diff_root = Some(worktree);
        self.diff_open = true;
    }
}

#[cfg(test)]
mod tests;
