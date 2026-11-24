use super::*;

pub(super) struct PendingDiscovery {
    reopen_workspace: bool,
    working_folder: Option<PathBuf>,
    rx: Receiver<Vec<PathBuf>>,
}

impl State {
    /// `atomics <path>` or `--workspace`, else the repository ATOMICS was started in, else, for a
    /// desktop started from a terminal, every repository discovered beneath the non-repo folder it was
    /// started in (bounded depth), else the projects and workspace from last time. A Dock, launcher or
    /// `atomics host` start without projects, and any start in `/`, the home folder or above it,
    /// reopens last time's. Discovery runs on a background thread so the window never waits on it. A
    /// start inside a repository still opens just that one, with no workspace settings applied to it.
    pub(super) fn start_initial_projects(&mut self, requested: Vec<PathBuf>, workspace: Option<String>) {
        self.project_discovery = None;
        self.working_folder = None;
        if let Some(name) = workspace {
            self.workspace = Some(name.clone());
            let mut projects = requested;
            match Workspace::load(&name) {
                Ok(ws) => projects.extend(ws.projects),
                Err(err) => self.toast(format!("{err:#}")),
            }
            self.add_resolved_projects(projects);
            self.remember_projects();
            return;
        }

        if !requested.is_empty() {
            self.workspace = None;
            let mut direct = Vec::new();
            let mut folders = Vec::new();
            for dir in requested {
                let resolved = if dir.is_absolute() { dir } else { self.launch_dir.join(dir) };
                if let Ok(root) = workspace::project_root(&resolved) {
                    if !direct.contains(&root) {
                        direct.push(root);
                    }
                } else if workspace::can_discover(&resolved) {
                    let folder = resolved.canonicalize().unwrap_or(resolved);
                    if !folders.contains(&folder) {
                        folders.push(folder);
                    }
                }
            }
            if !folders.is_empty() {
                for root in direct {
                    if !self.open_projects.contains(&root) {
                        self.open_projects.push(root);
                    }
                }
                let working_folder = (folders.len() == 1 && self.open_projects.is_empty()).then(|| folders[0].clone());
                self.spawn_project_discovery(folders, false, working_folder);
                return;
            }
            if !direct.is_empty() {
                for root in direct {
                    if !self.open_projects.contains(&root) {
                        self.open_projects.push(root);
                    }
                }
            } else {
                self.add_resolved_projects(self.store.projects.clone());
            }
            self.remember_projects();
            return;
        }

        // Bare start: no explicit path and no `--workspace`.
        if let Ok(root) = workspace::project_root(&self.launch_dir) {
            self.workspace = None;
            if !self.open_projects.contains(&root) {
                self.open_projects.push(root);
            }
            self.remember_projects();
            return;
        }

        if self.discover_launch_dir && workspace::can_discover(&self.launch_dir) {
            let folder = self.launch_dir.canonicalize().unwrap_or_else(|_| self.launch_dir.clone());
            self.spawn_project_discovery(vec![folder.clone()], true, Some(folder));
            return;
        }

        self.reopen_last_session(true);
    }

    fn spawn_project_discovery(
        &mut self,
        folders: Vec<PathBuf>,
        reopen_workspace: bool,
        working_folder: Option<PathBuf>,
    ) {
        let store_path = paths::store_file();
        if !store_path.exists() {
            let _ = self.store.save(&store_path);
        }
        let (tx, rx) = self.wake.channel();
        let _ = std::thread::Builder::new().name("atomics-discover".into()).spawn(move || {
            let mut discovered = Vec::new();
            for folder in folders {
                for root in workspace::discover_projects(&folder, workspace::DEFAULT_DISCOVERY_DEPTH) {
                    if !discovered.contains(&root) {
                        discovered.push(root);
                    }
                }
            }
            let _ = tx.send(discovered);
        });
        self.project_discovery = Some(PendingDiscovery { reopen_workspace, working_folder, rx });
    }

    /// Applies background working-folder repository discovery once its worker finishes.
    pub(super) fn poll_project_discovery(&mut self) -> bool {
        let Some(pending) = &self.project_discovery else { return false };
        let discovered = match pending.rx.try_recv() {
            Ok(discovered) => discovered,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Vec::new(),
        };
        let pending = self.project_discovery.take().expect("checked above");
        self.apply_discovered_projects(discovered, pending.reopen_workspace, pending.working_folder);
        true
    }

    fn apply_discovered_projects(
        &mut self,
        discovered: Vec<PathBuf>,
        reopen_workspace: bool,
        working_folder: Option<PathBuf>,
    ) {
        if !discovered.is_empty() {
            self.workspace = None;
            self.working_folder = working_folder;
            for root in discovered {
                if !self.open_projects.contains(&root) {
                    self.open_projects.push(root);
                }
            }
            self.remember_projects();
            self.apply_config();
            self.github_poll_now();
        } else if self.open_projects.is_empty() {
            self.reopen_last_session(reopen_workspace);
            if self.workspace.is_some() || !self.open_projects.is_empty() {
                self.apply_config();
                self.github_poll_now();
            }
        } else {
            self.remember_projects();
            self.apply_config();
        }
        if std::mem::take(&mut self.startup_after_discovery) {
            self.finish_startup();
        }
    }

    /// Resumes what a restart handed off and autostarts the chief now, or, while discovery runs, once it
    /// has opened the projects: the workspace's `[chief]` and `[tickets]` apply to them.
    pub(super) fn resume_after_projects(&mut self) {
        if self.project_discovery.is_some() {
            self.startup_after_discovery = true;
        } else {
            self.finish_startup();
        }
    }

    fn finish_startup(&mut self) {
        self.restore_restart();
        self.chief_autostart = self.chief_autostarts();
    }

    fn reopen_last_session(&mut self, reopen_workspace: bool) {
        let workspace = reopen_workspace
            .then(|| {
                let last = self.store.workspace.clone()?;
                Workspace::list().contains(&last).then_some(last)
            })
            .flatten();
        self.workspace = workspace.clone();
        let mut saved = Vec::new();
        if let Some(name) = &workspace {
            match Workspace::load(name) {
                Ok(ws) => saved = ws.projects,
                Err(err) => self.toast(format!("{err:#}")),
            }
        }
        let mut projects = self.store.projects.clone();
        // The projects left open last time are the reopened workspace's and what was added to it.
        if projects.is_empty() {
            projects = saved;
        }
        self.add_resolved_projects(projects);
        self.remember_projects();
    }

    fn add_resolved_projects(&mut self, projects: Vec<PathBuf>) {
        for dir in projects {
            if let Ok(root) = workspace::project_root(&dir)
                && !self.open_projects.contains(&root)
            {
                self.open_projects.push(root);
            }
        }
    }

    #[cfg(test)]
    pub(super) fn wait_for_project_discovery(&mut self) {
        let Some(pending) = self.project_discovery.take() else { return };
        let discovered = pending.rx.recv().unwrap_or_default();
        self.apply_discovered_projects(discovered, pending.reopen_workspace, pending.working_folder);
    }

    #[cfg(test)]
    pub(super) fn open_initial_projects(&mut self, requested: Vec<PathBuf>, workspace: Option<String>) {
        self.start_initial_projects(requested, workspace);
        self.wait_for_project_discovery();
    }

    /// The single project currently focused in the explorer or diff panel.
    pub(super) fn focused_project(&self) -> Option<PathBuf> {
        self.diff_target()
            .as_deref()
            .and_then(|target| self.project_of(target))
            .or_else(|| self.view.clone())
            .or_else(|| self.repos.first().cloned())
    }

    /// Repositories that need eager worktree, identity and workflow refreshes: all open repos when
    /// there are few, otherwise only focused, session-backed, plan-linked or diff-active repos. A session in a
    /// worktree beside its repository counts once git has named its project ([`Self::unmapped_session_roots`]).
    pub(super) fn active_repos(&self) -> Vec<PathBuf> {
        if self.repos.len() <= EAGER_REPO_LIMIT {
            return self.repos.clone();
        }
        let mut wanted = Vec::new();
        let mut push = |root: PathBuf| {
            if self.repos.contains(&root) && !wanted.contains(&root) {
                wanted.push(root);
            }
        };
        if let Some(focused) = self.focused_project() {
            push(focused);
        }
        if let Some(view) = &self.view {
            push(view.clone());
        }
        for session in &self.sessions {
            let root = session.root();
            if let Some(project) = self.project_of(&root).or_else(|| self.project_roots.get(&root).cloned()) {
                push(project);
            }
        }
        for plan in self.plans.iter().filter(|plan| plan.is_open()) {
            if let Some(project) = &plan.project {
                push(project.clone());
            }
            for task in &plan.tasks {
                if let Some(repo) = task.repo.as_deref().map(paths::expand) {
                    if let Some(root) = self.project_of(&repo) {
                        push(root);
                    } else {
                        push(repo);
                    }
                }
            }
        }
        for root in self.diffs.keys() {
            if let Some(project) = self.project_of(root) {
                push(project);
            }
        }
        wanted
    }

    /// Session folders whose project is unknown until git is asked: with many repositories, a worktree
    /// beside an idle repository, whose worktrees are listed only once it is active.
    pub(super) fn unmapped_session_roots(&self) -> Vec<PathBuf> {
        if self.repos.len() <= EAGER_REPO_LIMIT {
            return Vec::new();
        }
        let mut roots = Vec::new();
        for session in &self.sessions {
            let root = session.root();
            if self.project_of(&root).is_none() && !self.project_roots.contains_key(&root) && !roots.contains(&root) {
                roots.push(root);
            }
        }
        roots
    }

    fn remember_projects(&mut self) {
        for root in self.open_projects.clone() {
            self.store.touch_project(&root);
        }
        self.store.projects = self.open_projects.clone();
        self.store.workspace = self.workspace.clone();
        if let Some(current) = &self.workspace {
            self.store.recent_workspaces.retain(|name| name != current);
            self.store.recent_workspaces.insert(0, current.clone());
            self.store.recent_workspaces.truncate(24);
        }
        let _ = self.store.save_later(&paths::store_file());
        self.refresh_workspace_names();
    }

    /// The saved workspaces, read again: the host's state lists them for every client.
    pub(super) fn refresh_workspace_names(&mut self) {
        self.workspace_names = Workspace::list();
        self.workspace_names
            .sort_by_key(|name| self.store.recent_workspaces.iter().position(|s| s == name).unwrap_or(usize::MAX));
        self.workspace_projects = self
            .workspace_names
            .iter()
            .filter_map(|name| {
                let workspace = Workspace::load(name).ok()?;
                Some((name.clone(), workspace.projects))
            })
            .collect();
    }

    /// Why no ticket system is configured, as a ticket tool says it: the open workspace (or that none
    /// is, and which are saved), and what its `[tickets]` and the config's lack.
    pub(super) fn no_ticket_system(&self) -> String {
        let config = if self.config.tickets.system.trim().is_empty() {
            "config.toml has no [tickets] either".to_string()
        } else {
            format!("config.toml's [tickets] ({}) has no server", self.config.tickets.system)
        };
        let cause = match self.workspace.as_deref() {
            None => {
                let saved = Workspace::list();
                let names: Vec<String> = saved
                    .iter()
                    .map(|name| match Workspace::load(name).map(|ws| ws.extra.contains_key("tickets")) {
                        Ok(true) => format!("{name} (has [tickets])"),
                        _ => name.clone(),
                    })
                    .collect();
                let saved =
                    if names.is_empty() { "none saved".to_string() } else { format!("saved: {}", names.join(", ")) };
                format!(
                    "no workspace is open; {saved}. Open one from the Project menu, or start ATOMICS with --workspace NAME; {config}"
                )
            }
            Some(name) => match Workspace::load(name) {
                Err(error) => format!("the open workspace {name} does not load: {error:#}"),
                Ok(ws) => match ws.tickets() {
                    Err(error) => format!("the open workspace {name}'s [tickets] does not parse: {}", error.message()),
                    Ok(None) => format!("the open workspace {name} has no [tickets]; {config}"),
                    Ok(Some(_)) => format!("the open workspace {name}'s [tickets] names no system and server"),
                },
            },
        };
        format!("No ticket system is configured: {cause}.")
    }

    /// Adds a resolved project root without changing the desktop's view.
    pub(super) fn add_project(&mut self, root: &Path) {
        if !self.open_projects.iter().any(|project| project == root) {
            self.open_projects.push(root.to_path_buf());
        }
        self.remember_projects();
        self.apply_config();
    }

    pub(super) fn open_project(&mut self, dir: &Path) {
        match workspace::project_root(dir) {
            Ok(root) => {
                self.add_project(&root);
                self.active = None;
                self.diff_root = Some(root.clone());
                self.diff_open = true;
                self.toast(format!("Opened {}", paths::display(&root)));
            }
            Err(err) => self.toast(format!("{err:#}")),
        }
    }

    pub(super) fn close_project(&mut self, root: &Path) {
        self.open_projects.retain(|p| p != root);
        if self.open_projects.is_empty() {
            self.working_folder = None;
        }
        self.remember_projects();
        self.apply_config();
        if self.diff_root.as_deref().is_some_and(|d| self.project_of(d).as_deref() == Some(root) || d == root) {
            self.diff_root = None;
        }
    }

    /// Opens a saved workspace in place of the open projects; it is the one a start without
    /// `--workspace` and a restart open again. Returns how many projects it opened.
    pub(super) fn open_workspace(&mut self, name: &str) -> Result<usize, String> {
        match Workspace::load(name) {
            Ok(ws) => {
                self.project_discovery = None;
                self.working_folder = None;
                self.open_projects = ws.projects.iter().filter_map(|p| workspace::project_root(p).ok()).collect();
                self.workspace = Some(name.to_string());
                self.remember_projects();
                self.apply_config();
                if std::mem::take(&mut self.startup_after_discovery) {
                    self.finish_startup();
                }
                self.active = None;
                self.diff_root = self.open_projects.first().cloned();
                let opened = self.open_projects.len();
                self.toast(format!("Workspace {name}: {opened} projects"));
                Ok(opened)
            }
            Err(err) => {
                let error = format!("{err:#}");
                self.toast(error.clone());
                Err(error)
            }
        }
    }

    pub(super) fn save_workspace(&mut self, name: &str) {
        if let Err(error) = self.save_workspace_checked(name) {
            self.toast(error);
        }
    }

    pub(super) fn save_workspace_checked(&mut self, name: &str) -> Result<(), String> {
        let mut ws = match Workspace::load(name) {
            Ok(ws) => ws,
            Err(err)
                if err.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Workspace::default()
            }
            Err(err) => return Err(format!("{err:#}")),
        };
        ws.name = name.trim().to_string();
        ws.projects = self.open_projects.clone();
        let file = ws.save().map_err(|err| format!("{err:#}"))?;
        self.workspace = Some(ws.name);
        self.working_folder = None;
        self.remember_projects();
        self.apply_config();
        self.toast(format!("Saved {}", paths::display(&file)));
        Ok(())
    }

    /// The open project a folder belongs to.
    pub(super) fn project_of(&self, dir: &Path) -> Option<PathBuf> {
        if let Some(root) = self.repos.iter().find(|r| dir.starts_with(r)) {
            return Some(root.clone());
        }
        self.worktree_project
            .iter()
            .filter(|(worktree, _)| dir.starts_with(worktree))
            .max_by_key(|(worktree, _)| worktree.as_os_str().len())
            .map(|(_, project)| project.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::with_state;

    #[test]
    fn identity_form_scopes_contain_only_the_values_in_the_selected_file() {
        with_state(
            "app::projects::tests::identity_form_scopes_contain_only_the_values_in_the_selected_file",
            |mut state, _| {
                use atomics_core::identity::Identity;
                let (_dir, repo) = crate::app::tests::repository();
                let alias = paths::home().join("project-link");
                std::os::unix::fs::symlink(&repo, &alias).unwrap();
                state.config.identity.default = Identity {
                    name: Some("Main Example".into()),
                    email: Some("main@example.com".into()),
                    forbidden_words: Some(vec!["contoso".into()]),
                    github: None,
                };
                state.config.identity.projects.insert(
                    "~/project-link".into(),
                    Identity { name: Some("Project Example".into()), ..Default::default() },
                );
                std::fs::write(&state.config_path, toml::to_string(&state.config).unwrap()).unwrap();
                let host = Config::source_paths(&state.config_path)[1].clone();
                std::fs::create_dir_all(host.parent().unwrap()).unwrap();
                std::fs::write(host, "[identity]\nname = 'Host Example'\nemail = 'host@example.com'\n").unwrap();
                state.reload_config();
                let folder = workspace::workspaces_dir();
                std::fs::create_dir_all(&folder).unwrap();
                std::fs::write(
                    folder.join("scoped.toml"),
                    format!("projects = [{:?}]\n[identity]\nemail = 'workspace@example.com'\n", repo.to_string_lossy()),
                )
                .unwrap();
                state.open_workspace("scoped").unwrap();
                state.identity_updates.finish();
                let info = state.management_info();
                let scopes = info.config["identity_scopes"].as_array().unwrap();
                let main = scopes.iter().find(|scope| scope["target"] == "main").unwrap();
                assert_eq!(main["identity"]["name"], "Main Example", "host overrides are not the main file's prefill");
                assert_eq!(main["identity"]["forbidden_words"], json!(["contoso"]));
                let ws = scopes.iter().find(|scope| scope["target"] == "workspace").unwrap();
                assert_eq!(ws["identity"], json!({"email":"workspace@example.com"}));
                let canonical_repo = repo.canonicalize().unwrap();
                let project = scopes
                    .iter()
                    .find(|scope| scope["target"] == canonical_repo.to_string_lossy().as_ref())
                    .unwrap_or_else(|| panic!("project {} missing from scopes: {scopes:?}", canonical_repo.display()));
                assert_eq!(project["identity"], json!({"name":"Project Example"}));
                assert_eq!(project["path"], json!(["identity", "projects", "~/project-link"]));
                assert_eq!(info.tools["identities"][0]["override"], json!({"name":"Project Example"}));
                assert_eq!(
                    info.tools["identities"][0]["identity"]["email"], "workspace@example.com",
                    "display and editable values remain separate"
                );
                state.shutdown();
            },
        );
    }

    #[test]
    fn identity_is_known_despite_unrelated_workspace_errors_and_unknown_identity_blocks() {
        with_state(
            "app::projects::tests::identity_is_known_despite_unrelated_workspace_errors_and_unknown_identity_blocks",
            |mut state, _| {
                let (dir, repo) = crate::app::tests::repository();
                state.add_project(&repo);
                state.config.agents.push(atomics_core::config::AgentConfig {
                    name: "identity-fixture".into(),
                    command: vec!["/bin/sh".into(), "-c".into(), "exit 0".into()],
                    kind: AgentKind::Generic,
                    busy_text: None,
                    isolation: Some(atomics_core::isolation::IsolationMode::Bare),
                });
                let folder = workspace::workspaces_dir();
                std::fs::create_dir_all(&folder).unwrap();
                let file = folder.join("identity-errors.toml");
                let valid_identity = "[identity]\nname = 'Workspace Example'\nemail = 'workspace@example.com'\n";
                std::fs::write(&file, format!("projects = [{:?}]\n{valid_identity}", repo.to_string_lossy())).unwrap();
                state.open_workspace("identity-errors").unwrap();
                state.identity_updates.finish();
                // The complete workspace fails to deserialize, but its independent identity is valid.
                std::fs::write(&file, format!("projects = 7\n{valid_identity}")).unwrap();
                assert!(Workspace::load("identity-errors").is_err());
                state.apply_ticket_config();
                state.identity_updates.finish();
                assert!(state.tickets.workspace_identity_error.is_none());
                assert_eq!(state.project_identity(&repo).email.as_deref(), Some("workspace@example.com"));
                state.start("identity-fixture", repo.clone(), None, Vec::new(), None).unwrap();
                let tree = dir.path().join("task");
                git::add_worktree(&repo, &tree, "identity-task", "main").unwrap();
                std::fs::write(tree.join("safe.txt"), "safe\n").unwrap();
                state.plans[0].tasks = vec![Task {
                    id: "a".into(),
                    title: "Safe work".into(),
                    repo: Some(repo.to_string_lossy().into_owned()),
                    branch: Some("identity-task".into()),
                    worktree: Some(tree),
                    status: TaskStatus::Review,
                    reported_at: Some(git::report_time()),
                    ..Default::default()
                }];
                state.integrate(0, "a", None);
                let result = state
                    .integrate_rx
                    .recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(10)))
                    .unwrap();
                assert!(result.result.is_ok(), "{:?}", result.result);
                state.finish_integration(result);
                std::fs::write(&file, "projects = 7\n").unwrap();
                state.apply_ticket_config();
                assert!(state.tickets.workspace_identity.is_none());
                assert!(
                    state.tickets.workspace_identity_error.is_none(),
                    "missing identity is a known fallback even if projects is invalid"
                );
                for broken in ["projects = []\n[identity]\nemail = ''\n", "projects = []\n[identity\n"] {
                    std::fs::write(&file, broken).unwrap();
                    state.apply_ticket_config();
                    assert!(state.tickets.workspace_identity_error.is_some());
                    assert!(state.start("identity-fixture", repo.clone(), None, Vec::new(), None).is_err());
                    let (tx, rx) = channel();
                    state.integrate(0, "a", Some(tx));
                    let ControlOutcome::Done(answer) = rx.recv().unwrap() else { panic!("immediate answer") };
                    assert!(!answer.ok);
                }
                state.sessions.clear();
                state.identity_updates.finish();
                state.shutdown();
            },
        );
    }

    #[test]
    fn identity_follows_workspace_switches_and_user_project_overrides() {
        with_state(
            "app::projects::tests::identity_follows_workspace_switches_and_user_project_overrides",
            |mut state, _| {
                use atomics_core::identity::{Identity, set_worktree_identity};
                let (dir, repo) = crate::app::tests::repository();
                let alias = dir.path().join("parent-alias");
                std::os::unix::fs::symlink(dir.path(), &alias).unwrap();
                let task = alias.join("task");
                git::add_worktree(&repo, &task, "task", "main").unwrap();
                let identity = |name: &str, email: &str| Identity {
                    name: Some(name.into()),
                    email: Some(email.into()),
                    ..Default::default()
                };
                let main = identity("Main Example", "main@example.com");
                let project = identity("Project Example", "project@example.com");
                state.config.identity.default = main.clone();
                set_worktree_identity(&task, &main).unwrap();
                state.add_project(&repo);
                state.identity_updates.finish();
                let folder = workspace::workspaces_dir();
                std::fs::create_dir_all(&folder).unwrap();
                for (name, email) in [("one", "one@example.com"), ("two", "two@example.com")] {
                    std::fs::write(
                        folder.join(format!("{name}.toml")),
                        format!(
                            "projects = [{:?}]\n[identity]\nname = 'Workspace Example'\nemail = '{email}'\n",
                            repo.to_string_lossy()
                        ),
                    )
                    .unwrap();
                }
                for (name, email) in [("one", "one@example.com"), ("two", "two@example.com")] {
                    state.open_workspace(name).unwrap();
                    state.identity_updates.finish();
                    assert_eq!(state.project_config(&task).identity.default.email.as_deref(), Some(email));
                    assert_eq!(git::git(&task, &["config", "user.email"]).unwrap().trim(), email);
                    let info = state.management_info();
                    assert!(info.tools["identities"][0]["label"].as_str().unwrap().contains(email));
                }
                state.config.identity.projects.insert(repo.to_string_lossy().into_owned(), project.clone());
                state.apply_config();
                state.identity_updates.finish();
                assert_eq!(state.project_config(&task).identity.default, project);
                std::fs::write(&state.config_path, toml::to_string(&state.config).unwrap()).unwrap();
                let effective = state.config_get(Some(&repo.to_string_lossy()), Some("identity.email")).unwrap();
                assert_eq!(effective["values"]["identity.email"]["value"], "project@example.com");
                assert_eq!(effective["values"]["identity.email"]["source"], "main");
                state.open_workspace("one").unwrap();
                state.identity_updates.finish();
                assert_eq!(state.project_config(&task).identity.default, project);
                assert_eq!(git::git(&task, &["config", "user.email"]).unwrap().trim(), "project@example.com");
                std::fs::create_dir_all(repo.join(".atomics")).unwrap();
                std::fs::write(
                    repo.join(".atomics/config.toml"),
                    "[identity]\nname = 'Repo Example'\nemail = 'repo@example.com'\n",
                )
                .unwrap();
                state.apply_config();
                state.identity_updates.finish();
                assert_eq!(
                    state.project_config(&task).identity.default,
                    project,
                    "repository config cannot choose identity"
                );
                std::fs::write(
                    folder.join("broken.toml"),
                    format!("projects = [{:?}]\n[identity]\nemail = ''\n", repo.to_string_lossy()),
                )
                .unwrap();
                state.open_workspace("broken").unwrap();
                assert!(state.tickets.workspace_identity_error.is_some());
                assert!(
                    state
                        .start("shell", task.clone(), None, Vec::new(), None)
                        .unwrap_err()
                        .contains("Workspace identity")
                );
                assert!(
                    state.management_info().tools["identities"][0]["label"]
                        .as_str()
                        .unwrap()
                        .contains("Workspace identity")
                );
                state.config.identity.projects.clear();
                state.workspace = None;
                state.apply_config();
                state.identity_updates.finish();
                assert_eq!(state.project_config(&repo).identity.default, main);
                assert_eq!(git::git(&task, &["config", "user.email"]).unwrap().trim(), "main@example.com");
                assert_eq!(git::git(&repo, &["config", "user.email"]).unwrap().trim(), "test@example.org");
                state.plans[0].tasks.push(Task { worktree: Some(task.clone()), ..Default::default() });
                state.config.identity.default = Default::default();
                state.apply_config();
                state.identity_updates.finish();
                assert!(!atomics_core::identity::is_managed_worktree(&task));
                assert_eq!(git::git(&task, &["config", "user.email"]).unwrap().trim(), "test@example.org");
                state.config.identity.default = main;
                state.apply_config();
                state.identity_updates.finish();
                assert_eq!(
                    git::git(&task, &["config", "user.email"]).unwrap().trim(),
                    "main@example.com",
                    "saved task ownership survives removing its identity include"
                );
                state.shutdown();
            },
        );
    }

    #[test]
    fn a_start_that_names_nothing_opens_the_last_workspace_and_errors_name_the_cause() {
        with_state(
            "app::projects::tests::a_start_that_names_nothing_opens_the_last_workspace_and_errors_name_the_cause",
            |mut state, _| {
                let (_repo_dir, repo) = crate::app::tests::repository();
                let dir = workspace::workspaces_dir();
                std::fs::create_dir_all(&dir).unwrap();
                let team = format!(
                    "projects = [{:?}]\n[tickets]\nsystem = \"tracker\"\n[tickets.server]\nurl = \"https://tickets.example.com/mcp\"\n",
                    repo.display().to_string()
                );
                std::fs::write(dir.join("team.toml"), team).unwrap();
                std::fs::write(dir.join("daily.toml"), "projects = []\n").unwrap();
                state.refresh_workspace_names();

                // Started from the Dock with nothing open: the ticket tools say why there is no system.
                state.workspace = None;
                state.apply_config();
                assert!(state.ticket_config().is_none());
                let why = state.no_ticket_system();
                assert!(why.starts_with("No ticket system is configured: no workspace is open"), "{why}");
                assert!(why.contains("saved: daily, team (has [tickets])"), "{why}");
                assert!(why.contains("--workspace NAME"), "{why}");

                // Opening one from the picker remembers it for the next start.
                assert_eq!(state.open_workspace("team"), Ok(1));
                assert_eq!(Store::load(&paths::store_file()).workspace.as_deref(), Some("team"));
                assert_eq!(state.ticket_config().map(|c| c.system.clone()).as_deref(), Some("tracker"));
                let host = state.host_info();
                assert_eq!(host.workspace.as_deref(), Some("team"));
                assert_eq!(host.workspaces, ["team", "daily"], "the one opened last comes first");

                // The next start that names neither a project nor a workspace opens it again, with
                // the projects that were open.
                let open = state.open_projects.clone();
                let fresh = |state: &mut State, requested: Vec<PathBuf>, workspace: Option<String>| {
                    state.store = Store::load(&paths::store_file());
                    state.workspace = None;
                    state.open_projects.clear();
                    state.open_initial_projects(requested, workspace);
                    state.apply_config();
                };
                fresh(&mut state, vec![], None);
                assert_eq!(state.workspace.as_deref(), Some("team"));
                assert_eq!(state.open_projects, open);
                assert!(state.ticket_config().is_some(), "its [tickets] apply again");
                // Naming one wins; naming a project opens just that, as before.
                fresh(&mut state, vec![], Some("daily".into()));
                assert_eq!(state.workspace.as_deref(), Some("daily"));
                fresh(&mut state, vec![repo.clone()], None);
                let root = workspace::project_root(&repo).unwrap();
                assert_eq!((state.workspace.as_deref(), state.open_projects.clone()), (None, vec![root]));
                assert_eq!(Store::load(&paths::store_file()).workspace, None);

                // A start in a repository (`atomics` in a terminal there, or in a folder below its
                // root) opens just that one, with no workspace: the last one's settings stay with it,
                // and nothing is saved back that would bring them to this repository later.
                state.open_workspace("team").unwrap();
                let (_other_dir, other) = crate::app::tests::repository();
                let other_root = workspace::project_root(&other).unwrap();
                let below = other.join("src");
                std::fs::create_dir_all(&below).unwrap();
                for launch in [other.clone(), below] {
                    state.launch_dir = launch.clone();
                    fresh(&mut state, vec![], None);
                    let opened = (state.workspace.as_deref(), state.open_projects.clone());
                    assert_eq!(opened, (None, vec![other_root.clone()]), "{}", launch.display());
                    assert!(state.ticket_config().is_none(), "team's [tickets] stay with team");
                    let store = Store::load(&paths::store_file());
                    assert_eq!((store.workspace, store.projects), (None, vec![other_root.clone()]));
                    state.open_workspace("team").unwrap();
                }
                // Naming a project from a folder outside any repository does not reopen it either.
                let outside = paths::home().join("notes");
                std::fs::create_dir_all(&outside).unwrap();
                state.launch_dir = outside;
                fresh(&mut state, vec![other.clone()], None);
                assert_eq!((state.workspace.as_deref(), state.open_projects.clone()), (None, vec![other_root.clone()]));
                // From the Dock again, outside any repository, the workspace open last comes back.
                state.open_workspace("team").unwrap();
                fresh(&mut state, vec![], None);
                assert_eq!((state.workspace.as_deref(), state.open_projects.clone()), (Some("team"), open.clone()));

                // A workspace that is no longer saved is not opened again.
                state.open_workspace("team").unwrap();
                std::fs::remove_file(dir.join("team.toml")).unwrap();
                fresh(&mut state, vec![], None);
                assert_eq!(state.workspace, None);

                // An open workspace without [tickets] is named as the cause.
                state.workspace = Some("daily".into());
                let why = state.no_ticket_system();
                assert!(why.contains("the open workspace daily has no [tickets]"), "{why}");
            },
        );
    }

    #[test]
    fn saving_project_changes_keeps_workspace_ticket_settings() {
        with_state("app::projects::tests::saving_project_changes_keeps_workspace_ticket_settings", |mut state, _| {
            let dir = workspace::workspaces_dir();
            std::fs::create_dir_all(&dir).unwrap();
            let file = dir.join("daily.toml");
            std::fs::write(&file, "projects = []\n[tickets]\nprovider = 'relay'\nworkflow = 'personal'\n").unwrap();
            state.open_projects = vec![paths::home().join("repos/app")];
            state.save_workspace("daily");
            let saved = Workspace::load("daily").unwrap();
            assert_eq!(saved.projects, state.open_projects);
            assert_eq!(saved.extra["tickets"]["workflow"].as_str(), Some("personal"));
            assert!(std::fs::read_to_string(&file).unwrap().contains("~/repos/app"));

            std::fs::write(&file, "[broken").unwrap();
            state.save_workspace("daily");
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "[broken");
            assert!(state.toast.as_ref().unwrap().0.contains("parsing"));
        });
    }

    fn init_repo_at(path: &Path) -> PathBuf {
        std::fs::create_dir_all(path).unwrap();
        let run = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .arg("-C")
                    .arg(path)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&[
            "-c",
            "user.email=test@example.org",
            "-c",
            "user.name=Test",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ]);
        workspace::project_root(path).unwrap()
    }

    fn copy_dir_all(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let dest = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_dir_all(&entry.path(), &dest);
            } else {
                std::fs::copy(entry.path(), dest).unwrap();
            }
        }
    }

    #[test]
    fn non_repo_working_folder_opens_discovered_repos_falls_back_when_empty_and_keeps_single_repo_behavior() {
        with_state(
            "app::projects::tests::non_repo_working_folder_opens_discovered_repos_falls_back_when_empty_and_keeps_single_repo_behavior",
            |mut state, _| {
                let (_prev_dir, previous) = crate::app::tests::repository();
                let previous_root = workspace::project_root(&previous).unwrap();
                let ws_dir = workspace::workspaces_dir();
                std::fs::create_dir_all(&ws_dir).unwrap();
                std::fs::write(
                    ws_dir.join("last.toml"),
                    format!("projects = [{:?}]\n", previous_root.to_string_lossy()),
                )
                .unwrap();
                state.open_workspace("last").unwrap();

                let work = paths::home().join("work");
                std::fs::create_dir_all(&work).unwrap();
                let depth1 = init_repo_at(&work.join("alpha"));
                let depth2_a = init_repo_at(&work.join("repos/app"));
                let depth2_b = init_repo_at(&work.join("repos/backend"));
                let _depth3 = init_repo_at(&work.join("repos/nested/too-deep"));
                let _hidden = init_repo_at(&work.join(".hidden-repo"));
                let _hidden_parent = init_repo_at(&work.join(".private/repo"));
                let _target = init_repo_at(&work.join("target/generated"));
                let _node_modules = init_repo_at(&work.join("node_modules/pkg"));
                std::os::unix::fs::symlink(work.join("repos/app"), work.join("current")).unwrap();
                assert!(
                    std::process::Command::new("git")
                        .env("GIT_CONFIG_NOSYSTEM", "1")
                        .arg("-C")
                        .arg(&depth2_b)
                        .args(["worktree", "add", "-q", "-b", "wt-branch"])
                        .arg(work.join("backend-wt"))
                        .arg("main")
                        .status()
                        .unwrap()
                        .success()
                );

                let untrusted_dir = work.join("untrusted-share");
                let _untrusted_repo = init_repo_at(&untrusted_dir.join("secret"));
                workspace::distrust(&untrusted_dir, "written by something else");

                let mut expected = vec![depth1.clone(), depth2_a.clone(), depth2_b.clone()];
                expected.sort();

                let fresh = |state: &mut State, launch: &Path, requested: Vec<PathBuf>, ws: Option<String>| {
                    state.store = Store::load(&paths::store_file());
                    state.launch_dir = launch.to_path_buf();
                    state.workspace = None;
                    state.working_folder = None;
                    state.open_projects.clear();
                    state.open_initial_projects(requested, ws);
                    state.apply_config();
                };

                // A bare start from the Dock, a desktop launcher or `atomics host` never discovers: it
                // reopens last time's workspace and leaves the saved one as it was.
                state.discover_launch_dir = false;
                fresh(&mut state, &work, vec![], None);
                assert_eq!(state.workspace.as_deref(), Some("last"));
                assert_eq!(state.open_projects, vec![previous_root.clone()]);
                assert_eq!(state.working_folder, None);
                assert_eq!(Store::load(&paths::store_file()).workspace.as_deref(), Some("last"));

                // Neither does a terminal start in the home folder, `/` or a folder above home.
                state.discover_launch_dir = true;
                for launch in [paths::home(), PathBuf::from("/"), paths::home().parent().unwrap().to_path_buf()] {
                    fresh(&mut state, &launch, vec![], None);
                    assert!(state.project_discovery.is_none(), "{}", launch.display());
                    assert_eq!(state.workspace.as_deref(), Some("last"), "{}", launch.display());
                    assert_eq!(state.open_projects, vec![previous_root.clone()]);
                    assert_eq!(state.working_folder, None);
                }
                fresh(&mut state, &work, vec![paths::home()], None);
                assert_eq!(state.working_folder, None);
                state.open_workspace("last").unwrap();

                // A bare start from a terminal in the non-repo working folder discovers depth 1 and 2, dedupes
                // aliases/worktrees, skips hidden/target/node_modules/untrusted, remembers them, and clears the
                // prior workspace.
                fresh(&mut state, &work, vec![], None);
                assert_eq!(state.workspace, None);
                assert_eq!(state.open_projects, expected);
                assert_eq!(state.working_folder, Some(work.canonicalize().unwrap()));
                let remembered = Store::load(&paths::store_file());
                assert_eq!(remembered.projects, expected);
                assert_eq!(remembered.workspace, None);

                // Chief starts in the working folder itself (not an arbitrary first repo) when no
                // `[chief] cwd` or single-project view is selected.
                state.config.chief.command = vec!["/bin/sh".into(), "-c".into(), "exec sleep 30".into()];
                state.config.chief.args.clear();
                state.config.chief.cwd = None;
                let chief_id = state.start_chief(None).unwrap();
                crate::app::tests::wait_started(&mut state, &chief_id);
                let chief = state.sessions.iter().find(|s| s.id == chief_id).unwrap();
                assert_eq!(chief.cwd, work.canonicalize().unwrap());
                state.close_session(&chief_id, false);

                // Closing the last project, or saving the projects as a workspace, ends the working folder,
                // so the chief no longer starts there.
                for project in expected.clone() {
                    state.close_project(&project);
                }
                assert_eq!(state.working_folder, None);
                fresh(&mut state, &work, vec![], None);
                assert_eq!(state.working_folder, Some(work.canonicalize().unwrap()));
                state.save_workspace_checked("from-folder").unwrap();
                assert_eq!(state.working_folder, None);
                assert_eq!(state.workspace.as_deref(), Some("from-folder"));

                // Explicit `atomics <folder>` with a non-repo working folder discovers and opens its repos too.
                state.open_workspace("last").unwrap();
                let empty = paths::home().join("empty-launch");
                std::fs::create_dir_all(&empty).unwrap();
                fresh(&mut state, &empty, vec![work.clone()], None);
                assert_eq!(state.workspace, None);
                assert_eq!(state.open_projects, expected);
                assert_eq!(state.working_folder, Some(work.canonicalize().unwrap()));

                // A non-repo folder with no repos falls back to last session's workspace and projects.
                state.open_workspace("last").unwrap();
                fresh(&mut state, &empty, vec![], None);
                assert_eq!(state.workspace.as_deref(), Some("last"));
                assert_eq!(state.open_projects, vec![previous_root]);
                assert_eq!(state.working_folder, None);

                // A start inside a repo still opens only that repo (never sibling or nested repos).
                state.open_workspace("last").unwrap();
                let inside = depth1.join("sub");
                std::fs::create_dir_all(&inside).unwrap();
                for launch in [depth1.clone(), inside] {
                    fresh(&mut state, &launch, vec![], None);
                    assert_eq!(state.workspace, None);
                    assert_eq!(state.open_projects, vec![depth1.clone()]);
                    assert_eq!(state.working_folder, None);
                    state.open_workspace("last").unwrap();
                }

                state.shutdown();
            },
        );
    }

    #[test]
    fn eighty_fixture_repos_discover_off_the_window_thread_and_keep_per_project_work_lazy() {
        with_state(
            "app::projects::tests::eighty_fixture_repos_discover_off_the_window_thread_and_keep_per_project_work_lazy",
            |mut state, _| {
                let template_dir = paths::home().join("template-repo");
                init_repo_at(&template_dir);
                let template_git = template_dir.join(".git");

                let work = paths::home().join("work-80");
                let grouped = work.join("repos");
                std::fs::create_dir_all(&grouped).unwrap();
                for index in 0..80 {
                    let repo = if index < 10 {
                        work.join(format!("top-{index:02}"))
                    } else {
                        grouped.join(format!("repo-{index:02}"))
                    };
                    std::fs::create_dir_all(&repo).unwrap();
                    copy_dir_all(&template_git, &repo.join(".git"));
                }
                // Add symlink aliases (like ~/work/current -> repos/<repo>) that must dedupe.
                std::os::unix::fs::symlink(grouped.join("repo-10"), work.join("current")).unwrap();
                std::os::unix::fs::symlink(grouped.join("repo-79"), work.join("latest")).unwrap();

                state.open_projects.clear();
                state.repos.clear();
                state.diffs.clear();
                state.worktrees_listed_repos.clear();

                // Window startup calls `start_initial_projects`: it hands the 80-repo walk to a worker
                // and opens nothing itself.
                let start_call = Instant::now();
                state.start_initial_projects(vec![work.clone()], None);
                let launch_return = start_call.elapsed();
                assert!(state.project_discovery.is_some(), "discovery must run asynchronously off the window thread");
                assert!(state.open_projects.is_empty(), "start_initial_projects must not wait for discovery");

                // Drive runtime_tick until background discovery completes and measure total wall time.
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(30));
                while state.project_discovery.is_some() {
                    assert!(Instant::now() < deadline, "80-repo discovery did not finish in time");
                    state.runtime_tick();
                    std::thread::sleep(Duration::from_millis(2));
                }
                let discovery_elapsed = start_call.elapsed();
                assert_eq!(state.open_projects.len(), 80);
                assert_eq!(state.repos.len(), 80);
                assert_eq!(state.working_folder, Some(work.canonicalize().unwrap()));
                assert!(state.title.contains("80 projects") || state.title.is_empty());

                // Run ticks so worktree listing & diff scheduling happen; per-project work must
                // stay bounded to the focused project rather than fanning out to all 80 repos.
                let tick_start = Instant::now();
                state.runtime_tick();
                let tick_elapsed = tick_start.elapsed();
                let wait_listing = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                while state.worktree_listing.is_some() {
                    assert!(Instant::now() < wait_listing, "focused worktree listing did not finish");
                    state.runtime_tick();
                    std::thread::sleep(Duration::from_millis(2));
                }
                assert_eq!(state.active_repos().len(), 1, "only the focused repo should be active initially");
                assert_eq!(
                    state.diffs.len(),
                    1,
                    "idle repos must not all be queued for diffing: {:?}",
                    state.diffs.keys().collect::<Vec<_>>()
                );
                assert_eq!(state.worktrees_listed_repos.len(), 1, "worktree listing must stay bounded to active repos");
                assert!(
                    state.watcher.as_ref().map_or(0, |w| w.roots().len()) <= 1,
                    "watchers must stay bounded to active targets"
                );

                // Focusing a second repository lazily loads that repository on demand.
                let other = state.repos[42].clone();
                state.diff_root = Some(other.clone());
                state.runtime_tick();
                assert!(state.diffs.contains_key(&other), "newly focused repo must load lazily");
                assert!(state.diffs.len() <= 2, "only visited repos should be in diffs");

                eprintln!(
                    "80-repo working folder measurement: start_initial_projects={launch_return:?}, \
                     background_discovery={discovery_elapsed:?}, runtime_tick={tick_elapsed:?}, \
                     active_repos={}, diffs={}, watched={}",
                    state.active_repos().len(),
                    state.diffs.len(),
                    state.watcher.as_ref().map_or(0, |w| w.roots().len()),
                );
                state.shutdown();
            },
        );
    }

    #[test]
    fn empty_background_discovery_initializes_store_and_preserves_assigned_repos() {
        with_state(
            "app::projects::tests::empty_background_discovery_initializes_store_and_preserves_assigned_repos",
            |mut state, _| {
                let empty = paths::home().join("empty-work");
                std::fs::create_dir_all(&empty).unwrap();
                std::fs::remove_file(paths::store_file()).unwrap();
                state.start_initial_projects(vec![empty], None);
                // Store file exists immediately even when discovery runs in the background.
                assert!(state.project_discovery.is_some());
                assert!(paths::store_file().is_file(), "store file must be initialized during startup");

                let (_dir, repo) = crate::app::tests::repository();
                let peer = repo.parent().unwrap().join("repo-peer");
                assert!(
                    std::process::Command::new("git")
                        .env("GIT_CONFIG_NOSYSTEM", "1")
                        .arg("-C")
                        .arg(&repo)
                        .args(["worktree", "add", "-q", "-b", "peer"])
                        .arg(&peer)
                        .arg("main")
                        .status()
                        .unwrap()
                        .success()
                );

                // Assigning `repos` while empty initial discovery is pending must not be clobbered
                // when discovery finishes with nothing to open, and worktree listing must run for it.
                state.repos = vec![repo.clone()];
                state.worktrees_at = None;
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                while state.project_discovery.is_some() || state.worktrees.is_empty() {
                    assert!(Instant::now() < deadline, "worktree listing did not complete for assigned repo");
                    state.runtime_tick();
                    std::thread::sleep(Duration::from_millis(5));
                }
                assert_eq!(state.repos, vec![repo]);
                let listed: Vec<_> = state.worktrees.iter().map(|w| w.branch.clone().unwrap_or_default()).collect();
                assert_eq!(listed, ["main", "peer"]);
                state.shutdown();
            },
        );
    }

    #[test]
    fn a_restart_and_launch_opens_wait_for_discovery_and_the_workspace_it_reopens() {
        with_state(
            "app::projects::tests::a_restart_and_launch_opens_wait_for_discovery_and_the_workspace_it_reopens",
            |mut state, _| {
                let (_prev_dir, previous) = crate::app::tests::repository();
                let previous_root = workspace::project_root(&previous).unwrap();
                let ws_dir = workspace::workspaces_dir();
                std::fs::create_dir_all(&ws_dir).unwrap();
                std::fs::write(
                    ws_dir.join("last.toml"),
                    format!("projects = [{:?}]\n\n[chief]\nautostart = true\n", previous_root.to_string_lossy()),
                )
                .unwrap();
                state.open_workspace("last").unwrap();
                state.config.chief.autostart = false;

                // A terminal start in a folder with no repositories: discovery finds none and reopens the
                // last workspace, whose `[chief]` the restart and autostart must see.
                let empty = paths::home().join("empty-work");
                std::fs::create_dir_all(&empty).unwrap();
                state.store = Store::load(&paths::store_file());
                state.launch_dir = empty;
                state.discover_launch_dir = true;
                state.workspace = None;
                state.open_projects.clear();
                state.apply_config();
                state.store.restart = Some(atomics_core::store::RestartState {
                    old_version: "old".into(),
                    reason: Some("test".into()),
                    unplanned: true,
                    ..Default::default()
                });
                state.pending_open = vec![("no-such-agent".into(), previous_root.clone())];
                state.start_initial_projects(Vec::new(), None);
                // Hold the worker's answer, so the ticks below run while discovery is pending.
                let pending = state.project_discovery.take().expect("discovery runs");
                let (tx, rx) = state.wake.channel();
                state.project_discovery = Some(PendingDiscovery { rx, ..pending });
                state.resume_after_projects();
                state.runtime_tick();
                state.runtime_tick();
                assert!(state.store.restart.is_some() && state.restart_event.is_none(), "the restart waits");
                assert_eq!(state.pending_open.len(), 1, "launch opens wait");
                assert!(!state.chief_autostart);

                tx.send(Vec::new()).unwrap();
                assert!(state.poll_project_discovery());
                assert_eq!(state.workspace.as_deref(), Some("last"));
                assert_eq!(state.open_projects, vec![previous_root]);
                assert!(state.store.restart.is_none() && state.restart_event.is_some(), "the restart resumes");
                assert!(state.chief_autostart, "the reopened workspace's [chief] autostart applies");
                state.chief_autostart = false;
                state.runtime_tick();
                assert!(state.pending_open.is_empty());
                state.shutdown();
            },
        );
    }

    #[test]
    fn a_session_in_a_worktree_of_an_idle_repository_makes_it_active_in_a_large_workspace() {
        with_state(
            "app::projects::tests::a_session_in_a_worktree_of_an_idle_repository_makes_it_active_in_a_large_workspace",
            |mut state, _| {
                use atomics_core::config::AgentKind;
                use atomics_core::session::AgentStatus;

                let work = paths::home().join("many");
                let repos: Vec<PathBuf> = (0..EAGER_REPO_LIMIT + 2)
                    .map(|index| init_repo_at(&work.join(format!("repo-{index:02}"))))
                    .collect();
                let idle = repos[5].clone();
                let feature = work.join("repo-05-feature");
                assert!(
                    std::process::Command::new("git")
                        .env("GIT_CONFIG_NOSYSTEM", "1")
                        .arg("-C")
                        .arg(&idle)
                        .args(["worktree", "add", "-q", "-b", "feature"])
                        .arg(&feature)
                        .arg("main")
                        .status()
                        .unwrap()
                        .success()
                );
                let feature = feature.canonicalize().unwrap();
                state.open_projects = repos.clone();
                state.apply_config();
                state.diff_root = Some(repos[0].clone());
                state.sessions.push(crate::app::tests::session(
                    "agent",
                    AgentKind::Generic,
                    AgentStatus::Working,
                    &feature,
                ));

                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(20));
                while state.project_of(&feature).is_none() || state.worktree_listing.is_some() {
                    assert!(Instant::now() < deadline, "the session's project was never listed");
                    state.runtime_tick();
                    std::thread::sleep(Duration::from_millis(5));
                }
                assert_eq!(state.project_of(&feature), Some(idle.clone()));
                let active = state.active_repos();
                assert!(active.contains(&idle), "{active:?}");
                assert!(!active.contains(&repos[7]), "repositories without sessions stay lazy: {active:?}");
                assert!(state.worktrees_listed_repos.contains(&idle));
                assert!(state.git_identities.contains_key(&idle));
                assert!(
                    state.worktrees.iter().any(|worktree| worktree.path.canonicalize().ok() == Some(feature.clone()))
                );
                state.shutdown();
            },
        );
    }
}
