use super::*;

impl State {
    /// `atomics <path>` or `--workspace`, else the repository ATOMICS was started in, else
    /// the projects from last time. A bare start, which names neither a project nor a workspace and
    /// is not in a repository (from the Dock, say), opens the workspace that was open last time again,
    /// if it is still saved, so its `[tickets]`, workflow and chief settings apply as they did. A start
    /// in a repository opens just that one, as it always has: another repository's workspace settings
    /// never apply to it.
    pub(super) fn open_initial_projects(&mut self, requested: Vec<PathBuf>, workspace: Option<String>) {
        let bare = requested.is_empty() && workspace::project_root(&self.launch_dir).is_err();
        let reopened = workspace.is_none() && bare;
        let workspace = workspace.or_else(|| {
            let last = self.store.workspace.clone().filter(|_| reopened)?;
            Workspace::list().contains(&last).then_some(last)
        });
        self.workspace = workspace.clone();
        let mut projects = requested;
        let mut saved = Vec::new();
        if let Some(name) = &workspace {
            match Workspace::load(name) {
                Ok(ws) if reopened => saved = ws.projects,
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
        // The projects left open last time are the reopened workspace's and what was added to it.
        if projects.is_empty() {
            projects = saved;
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
        self.store.workspace = self.workspace.clone();
        let _ = self.store.save(&paths::store_file());
        self.refresh_workspace_names();
    }

    /// The saved workspaces, read again: the host's state lists them for every client.
    pub(super) fn refresh_workspace_names(&mut self) {
        self.workspace_names = Workspace::list();
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
                self.open_projects = ws.projects.iter().filter_map(|p| workspace::project_root(p).ok()).collect();
                self.workspace = Some(name.to_string());
                self.remember_projects();
                self.apply_config();
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
                assert_eq!(host.workspaces, ["daily", "team"]);

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
}
