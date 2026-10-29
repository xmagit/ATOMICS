//! Desktop actions are in-process commands. IDs are resolved when the host runs
//! them, so a session or plan disappearing between paint and execution is safe.
use super::*;

pub(super) type Action = Box<dyn FnOnce(&mut State) + Send>;

impl State {
    pub(super) fn drain_actions(&mut self) -> bool {
        let mut changed = false;
        while let Ok(action) = self.action_rx.try_recv() {
            action(self);
            changed = true;
        }
        changed
    }
}

impl Desktop<'_> {
    pub(super) fn post(&self, action: impl FnOnce(&mut State) + Send + 'static) {
        let _ = self.runtime.action_tx.send(Box::new(action));
    }

    pub(super) fn post_plan(&self, index: usize, action: impl FnOnce(&mut State, usize) + Send + 'static) {
        let id = self.runtime.plans[index].id.clone();
        self.post(move |state| {
            if let Some(index) = state.plans.iter().position(|p| p.id == id) {
                action(state, index);
            }
        });
    }

    pub(super) fn post_session(&self, index: usize, action: impl FnOnce(&mut State, usize) + Send + 'static) {
        let id = self.runtime.sessions[index].id.clone();
        self.post(move |state| {
            if let Some(index) = state.find(&id) {
                action(state, index);
            }
        });
    }

    pub(super) fn activate(&mut self, id: &str, open_diff: bool) {
        // Selection is immediate client state, projected into the native model
        // for ctl and host subscription/scheduling decisions.
        self.runtime.active = Some(id.to_string());
        self.runtime.diff_root = None;
        self.runtime.diff_open |= open_diff;
        if self.runtime.sessions.iter().any(|s| s.id == id && s.terminal.is_some()) {
            self.client.focus_terminal = Some(id.to_string());
        }
        let id = id.to_string();
        self.post(move |state| {
            if let Some(session) = state.sessions.iter_mut().find(|s| s.id == id)
                && session.status == AgentStatus::Done
            {
                session.status = AgentStatus::Idle;
            }
        });
    }

    pub(super) fn close(&self, id: &str) {
        let id = id.to_string();
        self.post(move |state| state.close(&id));
    }

    pub(super) fn edit(&self, _ctx: &egui::Context, root: &Path, path: Option<&str>, line: Option<u32>) {
        let (root, path) = (root.to_path_buf(), path.map(str::to_string));
        self.post(move |state| state.edit(&state.ctx.clone(), &root, path.as_deref(), line));
    }

    pub(super) fn open_file(&self, root: &Path, path: &str) {
        let (root, path) = (root.to_path_buf(), path.to_string());
        self.post(move |state| state.open_file(&root, &path));
    }

    pub(super) fn send_review(&self, root: &Path) {
        let root = root.to_path_buf();
        self.post(move |state| {
            state.send_review(&root);
        });
    }

    pub(super) fn create_worktree(&self, repo: PathBuf, branch: String, start: Option<String>, launch: Launch) {
        self.post(move |state| state.create_worktree(repo, branch, start, launch));
    }

    pub(super) fn toggle_chief(&self, _ctx: &egui::Context) {
        self.post(|state| state.toggle_chief(&state.ctx.clone()));
    }

    pub(super) fn set_chief_dock(&self, dock: ChiefDock) {
        self.post(move |state| state.set_chief_dock(dock));
    }

    pub(super) fn save_chief_preferences(&self) {
        self.post(State::save_chief_preferences);
    }

    pub(super) fn open_project(&self, root: &Path) {
        let root = root.to_path_buf();
        self.post(move |state| state.open_project(&root));
    }

    pub(super) fn close_project(&self, root: &Path) {
        let root = root.to_path_buf();
        self.post(move |state| state.close_project(&root));
    }

    pub(super) fn pick_project(&self) {
        let start =
            self.runtime.repos.first().and_then(|r| r.parent()).map(Path::to_path_buf).unwrap_or_else(paths::home);
        if let Some(root) = rfd::FileDialog::new().set_title("Open project").set_directory(start).pick_folder() {
            self.open_project(&root);
        }
    }

    pub(super) fn open_workspace(&self, name: &str) {
        let name = name.to_string();
        self.post(move |state| state.open_workspace(&name));
    }

    pub(super) fn save_workspace(&self, name: &str) {
        let name = name.to_string();
        self.post(move |state| state.save_workspace(&name));
    }

    pub(super) fn open_cleanup(&self, repo: PathBuf, preselect: Option<PathBuf>) {
        self.post(move |state| state.open_cleanup(repo, preselect));
    }

    pub(super) fn open_plan_cleanup(&self, index: usize) {
        let plan = self.runtime.plans[index].id.clone();
        self.post(move |state| {
            if let Some(index) = state.plans.iter().position(|p| p.id == plan) {
                state.open_plan_cleanup(index);
            }
        });
    }

    pub(super) fn task_action(&mut self, plan: usize, task: usize, action: &str) {
        if action == "diff" {
            self.runtime.task_action(plan, task, action);
            return;
        }
        let (plan, task, action) =
            (self.runtime.plans[plan].id.clone(), self.runtime.plans[plan].tasks[task].id.clone(), action.to_string());
        self.post(move |state| {
            if let Some(plan) = state.plans.iter().position(|p| p.id == plan)
                && let Some(task) = state.plans[plan].tasks.iter().position(|t| t.id == task)
            {
                state.task_action(plan, task, &action);
            }
        });
    }
}
