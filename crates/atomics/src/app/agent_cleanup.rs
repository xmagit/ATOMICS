//! Background checks shared by accepted-task cleanup and the chief's idle wake.
use super::*;

const RECHECK: Duration = Duration::from_secs(30);

#[derive(Clone, PartialEq, Eq)]
struct Work {
    root: PathBuf,
    base: String,
    turn_ended: Option<Instant>,
    revision: u64,
    files: u64,
    task: Option<(String, String, TaskStatus)>,
}

struct Checked {
    work: Work,
    at: Instant,
    result: Result<Health, String>,
}

struct Health {
    clean: bool,
    merged: bool,
    empty: bool,
}

#[derive(Default)]
pub(super) struct Checks {
    pending: HashMap<String, Receiver<Checked>>,
    checked: HashMap<String, Checked>,
    pub revision: u64,
    pub files: Arc<Mutex<HashMap<PathBuf, u64>>>,
}

/// Reports can arrive before the agent's turn ends, especially in a sandbox.
fn idle(session: &Session) -> bool {
    !session.chief
        && session.is_agent()
        && matches!(session.status, AgentStatus::Idle | AgentStatus::Done)
        && !session.screen_busy
        && session.pending.is_none()
        && session.held.is_empty()
        && !session.approval_hold
        && session.approvals.is_empty()
        && session.worktree_ready.is_none()
}

impl State {
    fn cleanup_work(&self, session: &Session) -> Work {
        let root = session.root();
        let owner = self.plans.iter().find_map(|plan| {
            let task = plan.tasks.iter().find(|task| task.session.as_deref() == Some(&session.id))?;
            Some((plan, task))
        });
        let files = self.agent_cleanup.files.lock().unwrap().get(&root).copied().unwrap_or(0);
        Work {
            base: owner
                .filter(|(_, task)| task.id == plan::REVIEW_TASK)
                .map(|(plan, _)| plan.integration_branch())
                .unwrap_or_else(|| self.project_base_ref(&root)),
            root,
            turn_ended: session.activity.turn_ended,
            revision: self.integration_revision,
            files,
            task: owner.map(|(plan, task)| (plan.id.clone(), task.id.clone(), task.status)),
        }
    }

    pub(super) fn refresh_agent_cleanup(&mut self) {
        self.agent_cleanup.pending.retain(|id, _| self.sessions.iter().any(|s| s.id == *id));
        self.agent_cleanup.checked.retain(|id, _| self.sessions.iter().any(|s| s.id == *id));
        let ready: Vec<_> = self
            .agent_cleanup
            .pending
            .iter()
            .filter_map(|(id, rx)| match rx.try_recv() {
                Ok(checked) => Some((id.clone(), Some(checked))),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Some((id.clone(), None)),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
            })
            .collect();
        for (id, checked) in ready {
            self.agent_cleanup.revision += 1;
            self.agent_cleanup.pending.remove(&id);
            if let Some(checked) = checked {
                self.agent_cleanup.checked.insert(id, checked);
            }
        }
        for session in self.sessions.iter().filter(|s| idle(s)) {
            let work = self.cleanup_work(session);
            if self.agent_cleanup.pending.contains_key(&session.id)
                || self
                    .agent_cleanup
                    .checked
                    .get(&session.id)
                    .is_some_and(|checked| checked.work == work && checked.at.elapsed() < RECHECK)
            {
                continue;
            }
            let (tx, rx) = self.wake.channel();
            self.agent_cleanup.pending.insert(session.id.clone(), rx);
            std::thread::spawn(move || {
                let result = health(&work.root, &work.base).map_err(|err| format!("{err:#}"));
                let _ = tx.send(Checked { work, at: Instant::now(), result });
            });
        }
    }

    fn agent_health(&self, session: &Session) -> Option<&Result<Health, String>> {
        let checked = self.agent_cleanup.checked.get(&session.id)?;
        (idle(session)
            && !self.integration_dirty.load(Ordering::Relaxed)
            && checked.work == self.cleanup_work(session)
            && checked.at.elapsed() < RECHECK)
            .then_some(&checked.result)
    }

    pub(super) fn accepted_agent_finished(&self, session: &Session, task: &Task, plan: &Plan) -> bool {
        let Some(Ok(health)) = self.agent_health(session) else { return false };
        health.clean
            && task.status == TaskStatus::Done
            && (health.merged
                || health.empty
                || task.integration_verified && task.integrated.as_deref() == Some(&plan.integration_branch())
                || task.stages.iter().chain(&plan.stages).any(|stage| stage.stage == "merged"))
    }

    /// A closed board's agent is finished with once it is idle and its worktree clean, its task accepted or not:
    /// closing it loses nothing, and its task can still resume it.
    pub(super) fn closed_board_agent_finished(&self, session: &Session, plan: &Plan) -> bool {
        !plan.is_open() && !session.chief && matches!(self.agent_health(session), Some(Ok(Health { clean: true, .. })))
    }

    pub(super) fn agent_cleanup_note(&self, task: &Task) -> Option<String> {
        if task.status != TaskStatus::Done {
            return None;
        }
        let session = self.sessions.iter().find(|s| task.session.as_deref() == Some(&s.id) && !s.chief)?;
        Some(match self.agent_health(session) {
            Some(Ok(Health { clean: false, .. })) => "Agent kept open: worktree has uncommitted work".into(),
            Some(Err(_)) => "Agent kept open: worktree could not be checked for uncommitted work".into(),
            _ if !idle(session) => "Agent kept open: waiting for its turn and pending messages to finish".into(),
            None => "Agent kept open: checking worktree for uncommitted work".into(),
            _ => return None,
        })
    }

    /// A bounded list for the chief to act on; ungrouped sessions are never closed here.
    pub(super) fn agent_cleanup_candidates(&self) -> Vec<String> {
        self.sessions
            .iter()
            .filter_map(|session| {
                let Ok(health) = self.agent_health(session)? else { return None };
                if !health.clean {
                    return None;
                }
                let task = self
                    .plans
                    .iter()
                    .flat_map(|plan| &plan.tasks)
                    .find(|task| task.session.as_deref() == Some(&session.id));
                // A task still open is the chief's to accept. Its branch tells nothing: with no commits yet, HEAD is
                // at the merge base just as a merged branch's is, and the worker may be waiting for an answer.
                let reason = match task {
                    Some(task) if task.status == TaskStatus::Done => "accepted task",
                    Some(_) => return None,
                    None if health.merged => "merged branch",
                    None if health.empty => "no task or changes",
                    None => return None,
                };
                Some(format!("{} ({reason})", session.id))
            })
            .collect()
    }
}

#[cfg(test)]
pub(super) fn finish_checks(state: &mut State) {
    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
    state.finish_plan_agents();
    while !state.agent_cleanup.pending.is_empty() {
        assert!(Instant::now() < deadline, "agent cleanup checks did not finish");
        std::thread::sleep(Duration::from_millis(5));
        state.finish_plan_agents();
    }
}

fn health(root: &Path, base: &str) -> anyhow::Result<Health> {
    // Git's pinned command builder refuses relocated or tampered sandbox worktrees.
    // Outside git, only an entirely empty folder can count as having no work.
    let status = git::git(root, &["status", "--porcelain", "--untracked-files=all"]);
    let clean = match status {
        Ok(status) => status.is_empty(),
        Err(_) if std::fs::read_dir(root)?.next().is_none() => {
            return Ok(Health { clean: true, merged: false, empty: true });
        }
        Err(error) => return Err(error),
    };
    if !clean {
        return Ok(Health { clean: false, merged: false, empty: false });
    }
    let (base, _) = git::merge_base(root, base)?;
    if base == "(no commits)" {
        return Ok(Health { clean: true, merged: false, empty: true });
    }
    let merged = git::is_ancestor(root, "HEAD", &base)? == Some(true);
    let empty = git::git(root, &["diff", "--name-only", &base, "HEAD"])?.is_empty();
    Ok(Health { clean, merged, empty })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{repository, session, with_app};

    fn commit(root: &Path) {
        std::fs::write(root.join("feature"), "new work\n").unwrap();
        git::git(root, &["add", "."]).unwrap();
        git::git(root, &["commit", "-qm", "feature"]).unwrap();
    }

    #[test]
    fn accepted_merged_landed_and_empty_work_closes_into_recent() {
        with_app("app::agent_cleanup::tests::accepted_merged_landed_and_empty_work_closes_into_recent", |app, _| {
            app.runtime.config.cleanup.completed_plans = false;
            app.runtime.config.base_ref = "main".into();
            for outcome in ["merged", "landed", "plan-landed", "empty", "integrated", "unmerged"] {
                let (_dir, repo) = repository();
                let work = repo.with_file_name("work");
                git::add_worktree(&repo, &work, "task", "main").unwrap();
                app.runtime.repos = vec![repo.clone()];
                app.runtime.worktree_project.insert(work.clone(), repo.clone());
                if outcome != "empty" {
                    commit(&work);
                }
                if outcome == "merged" {
                    git::git(&repo, &["merge", "--ff-only", "task"]).unwrap();
                }
                app.runtime.sessions = vec![session(outcome, AgentKind::Claude, AgentStatus::Done, &work)];
                let mut task = Task {
                    id: "code".into(),
                    branch: Some("task".into()),
                    session: Some(outcome.into()),
                    status: TaskStatus::Done,
                    ..Task::default()
                };
                if outcome == "landed" {
                    task.stages.push(atomics_core::workflow::Stage { stage: "merged".into(), ..Default::default() });
                }
                if outcome == "integrated" {
                    task.integrated = Some("integration".into());
                    task.integration_verified = true;
                }
                app.runtime.plans = vec![Plan {
                    id: outcome.into(),
                    integration: Some("integration".into()),
                    tasks: vec![task],
                    ..Plan::default()
                }];
                if outcome == "plan-landed" {
                    app.runtime.plans[0]
                        .stages
                        .push(atomics_core::workflow::Stage { stage: "merged".into(), ..Default::default() });
                }
                app.runtime.selected_plan = 0;
                finish_checks(app.runtime);
                let closes = outcome != "unmerged";
                assert_eq!(app.runtime.sessions.is_empty(), closes, "{outcome}");
                assert_eq!(app.runtime.plans[0].tasks[0].finished_at.is_some(), closes, "{outcome}");
                assert_eq!(app.runtime.plans[0].status == PlanStatus::Completed, closes, "{outcome}");
                assert_eq!(
                    app.runtime.backend().recent_sessions().iter().any(|s| s.record.claude_session_id == outcome),
                    closes,
                    "{outcome}"
                );
                assert!(work.is_dir());
            }
        });
    }

    #[test]
    fn accepted_dirty_work_stays_open_and_the_card_explains() {
        with_app("app::agent_cleanup::tests::accepted_dirty_work_stays_open_and_the_card_explains", |app, ctx| {
            app.runtime.config.base_ref = "main".into();
            for dirty in ["untracked", "staged", "unstaged"] {
                let (_dir, repo) = repository();
                commit(&repo);
                if dirty == "untracked" {
                    std::fs::write(repo.join("new-file"), "unfinished\n").unwrap();
                } else {
                    std::fs::write(repo.join("feature"), "unfinished\n").unwrap();
                    if dirty == "staged" {
                        git::git(&repo, &["add", "."]).unwrap();
                    }
                }
                app.runtime.sessions = vec![session("dirty", AgentKind::Claude, AgentStatus::Idle, &repo)];
                app.runtime.plans = vec![Plan {
                    id: dirty.into(),
                    tasks: vec![Task {
                        id: "code".into(),
                        status: TaskStatus::Done,
                        branch: Some("main".into()),
                        session: Some("dirty".into()),
                        stages: vec![atomics_core::workflow::Stage { stage: "merged".into(), ..Default::default() }],
                        ..Task::default()
                    }],
                    ..Plan::default()
                }];
                app.runtime.selected_plan = 0;
                finish_checks(app.runtime);
                assert!(app.runtime.find("dirty").is_some());
                assert!(app.runtime.agent_cleanup_candidates().is_empty());
                let board = app.runtime.backend().board(0).unwrap();
                assert!(board.integration_notes["code"].0.contains("uncommitted work"));
                let output = ctx.run_ui(Default::default(), |ui| {
                    app.plan_panel(ui, ui.max_rect());
                });
                assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
                    egui::Shape::Text(text) if text.galley.text().contains("uncommitted work"))));
                output.drop_without_applying_deltas();
                // Plan completion must not bypass the same protection.
                app.runtime.close_plan(0, PlanStatus::Completed).unwrap();
                assert!(app.runtime.find("dirty").is_some());
            }
        });
    }

    #[test]
    fn acceptance_waits_for_a_turn_to_end_and_for_a_successful_worktree_check() {
        with_app(
            "app::agent_cleanup::tests::acceptance_waits_for_a_turn_to_end_and_for_a_successful_worktree_check",
            |app, _| {
                let empty = crate::test_support::tempdir().unwrap();
                app.runtime.config.cleanup.completed_plans = false;
                app.runtime.sessions = vec![session("busy", AgentKind::Claude, AgentStatus::Working, empty.path())];
                app.runtime.plans = vec![Plan {
                    id: "busy".into(),
                    tasks: vec![Task {
                        id: "code".into(),
                        session: Some("busy".into()),
                        status: TaskStatus::Done,
                        ..Task::default()
                    }],
                    ..Plan::default()
                }];
                for (status, busy) in [(AgentStatus::Working, false), (AgentStatus::Done, true)] {
                    app.runtime.sessions[0].status = status;
                    app.runtime.sessions[0].screen_busy = busy;
                    finish_checks(app.runtime);
                    assert!(app.runtime.find("busy").is_some());
                    assert!(app.runtime.agent_cleanup_candidates().is_empty());
                }
                app.runtime.sessions[0].screen_busy = false;
                app.runtime.sessions[0].cwd = empty.path().join("missing");
                finish_checks(app.runtime);
                assert!(app.runtime.find("busy").is_some());
                assert!(
                    app.runtime
                        .agent_cleanup_note(&app.runtime.plans[0].tasks[0])
                        .unwrap()
                        .contains("could not be checked")
                );
                app.runtime.sessions[0].cwd = empty.path().into();
                finish_checks(app.runtime);
                assert!(app.runtime.find("busy").is_none());
            },
        );
    }

    #[test]
    fn wake_cleanup_lists_finished_ungrouped_agents_without_closing_them() {
        with_app(
            "app::agent_cleanup::tests::wake_cleanup_lists_finished_ungrouped_agents_without_closing_them",
            |app, _| {
                let (_dir, repo) = repository();
                app.runtime.config.base_ref = "main".into();
                let merged = repo.with_file_name("merged");
                git::add_worktree(&repo, &merged, "merged", "main").unwrap();
                commit(&merged);
                git::git(&repo, &["merge", "--ff-only", "merged"]).unwrap();
                let dirty = repo.with_file_name("dirty");
                git::add_worktree(&repo, &dirty, "dirty", "main").unwrap();
                std::fs::write(dirty.join("new-file"), "unfinished").unwrap();
                let empty = repo.with_file_name("empty");
                git::add_worktree(&repo, &empty, "empty", "main").unwrap();
                git::git(&empty, &["commit", "--allow-empty", "-qm", "no changes"]).unwrap();
                let scratch = crate::test_support::tempdir().unwrap();
                let unmerged = repo.with_file_name("unmerged");
                git::add_worktree(&repo, &unmerged, "unmerged", "main").unwrap();
                std::fs::write(unmerged.join("other"), "new work").unwrap();
                git::git(&unmerged, &["add", "."]).unwrap();
                git::git(&unmerged, &["commit", "-qm", "other"]).unwrap();
                app.runtime.sessions = vec![
                    session("merged", AgentKind::Claude, AgentStatus::Done, &merged),
                    session("empty", AgentKind::Generic, AgentStatus::Idle, &empty),
                    session("dirty", AgentKind::Claude, AgentStatus::Idle, &dirty),
                    session("working", AgentKind::Claude, AgentStatus::Working, &merged),
                    session("turn-running", AgentKind::Claude, AgentStatus::Done, &merged),
                    session("unmerged", AgentKind::Claude, AgentStatus::Idle, &unmerged),
                    session("accepted", AgentKind::Claude, AgentStatus::Idle, &unmerged),
                    session("chief", AgentKind::Claude, AgentStatus::Idle, &merged),
                    session("shell", AgentKind::Shell, AgentStatus::Idle, scratch.path()),
                    session("scratch", AgentKind::Generic, AgentStatus::Idle, scratch.path()),
                ];
                app.runtime.sessions[4].screen_busy = true;
                app.runtime.sessions[7].chief = true;
                app.runtime.config.plan.close_done_agents = false;
                app.runtime.plans = vec![Plan {
                    id: "accepted".into(),
                    tasks: vec![Task {
                        id: "code".into(),
                        status: TaskStatus::Done,
                        branch: Some("unmerged".into()),
                        session: Some("accepted".into()),
                        ..Task::default()
                    }],
                    ..Plan::default()
                }];
                finish_checks(app.runtime);
                assert_eq!(
                    app.runtime.agent_cleanup_candidates(),
                    [
                        "merged (merged branch)",
                        "empty (no task or changes)",
                        "accepted (accepted task)",
                        "scratch (no task or changes)"
                    ]
                );
                assert_eq!(app.runtime.sessions.len(), 10, "the chief decides which ungrouped agents to close");
                // A turn or a filesystem change invalidates an earlier clean result immediately.
                app.runtime.sessions[0].status = AgentStatus::Working;
                assert_eq!(app.runtime.agent_cleanup_candidates().len(), 3);
                *app.runtime.agent_cleanup.files.lock().unwrap().entry(empty).or_default() += 1;
                assert_eq!(app.runtime.agent_cleanup_candidates().len(), 2);
                app.runtime.integration_dirty.store(true, Ordering::Relaxed);
                assert!(app.runtime.agent_cleanup_candidates().is_empty());
            },
        );
    }

    #[test]
    fn a_worker_with_a_task_and_no_commits_is_not_listed_as_a_merged_branch() {
        with_app(
            "app::agent_cleanup::tests::a_worker_with_a_task_and_no_commits_is_not_listed_as_a_merged_branch",
            |app, _| {
                let (_dir, repo) = repository();
                app.runtime.config.base_ref = "main".into();
                app.runtime.config.plan.close_done_agents = false;
                app.runtime.config.cleanup.completed_plans = false;
                let work = repo.with_file_name("work");
                git::add_worktree(&repo, &work, "task", "main").unwrap();
                app.runtime.sessions = vec![session("worker", AgentKind::Claude, AgentStatus::Idle, &work)];
                app.runtime.plans = vec![Plan {
                    id: "plan".into(),
                    tasks: vec![Task {
                        id: "code".into(),
                        branch: Some("task".into()),
                        session: Some("worker".into()),
                        status: TaskStatus::Running,
                        ..Task::default()
                    }],
                    ..Plan::default()
                }];
                for status in [TaskStatus::Planned, TaskStatus::Running, TaskStatus::Blocked, TaskStatus::Review] {
                    app.runtime.plans[0].tasks[0].status = status;
                    finish_checks(app.runtime);
                    assert_eq!(app.runtime.agent_cleanup_candidates(), Vec::<String>::new(), "{status:?}");
                }
                // Its branch landing does not change who decides while the task is open: the chief accepts it.
                commit(&work);
                git::git(&repo, &["merge", "--ff-only", "task"]).unwrap();
                *app.runtime.agent_cleanup.files.lock().unwrap().entry(work.clone()).or_default() += 1;
                finish_checks(app.runtime);
                assert_eq!(app.runtime.agent_cleanup_candidates(), Vec::<String>::new(), "landed, not accepted");
                // Accepted, it is the chief's to close, and with no task a merged branch still is.
                app.runtime.plans[0].tasks[0].status = TaskStatus::Done;
                *app.runtime.agent_cleanup.files.lock().unwrap().entry(work.clone()).or_default() += 1;
                finish_checks(app.runtime);
                assert_eq!(app.runtime.agent_cleanup_candidates(), ["worker (accepted task)"]);
                app.runtime.plans.clear();
                *app.runtime.agent_cleanup.files.lock().unwrap().entry(work).or_default() += 1;
                finish_checks(app.runtime);
                assert_eq!(app.runtime.agent_cleanup_candidates(), ["worker (merged branch)"]);
            },
        );
    }
}
