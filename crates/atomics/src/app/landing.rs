use super::*;
use anyhow::{Context, ensure};
use serde_json::Value;

impl State {
    /// Landing outlives an MCP call. The chief reads its result with plan_land status=true;
    /// a plan event wakes waiters on completion. No implicit retry can advance the target.
    pub(super) fn land(
        &mut self,
        index: usize,
        ids: Vec<String>,
        route: bool,
        override_busy: bool,
        reply: Sender<ControlOutcome>,
    ) {
        let setup = (|| -> Result<_, String> {
            let plan = &self.plans[index];
            if !plan.approved || plan.status == PlanStatus::Archived {
                return Err("Approve the plan and reopen it if archived before landing.".into());
            }
            if self.landing.get(&plan.id).is_some_and(|job| job["running"] == true)
                || self.integrating.iter().any(|(id, _)| id == &plan.id)
            {
                return Err("This plan already has an integration or landing in progress.".into());
            }
            if ids.is_empty() || ids.len() > 64 {
                return Err("Select between 1 and 64 task ids to land.".into());
            }
            let mut tasks = Vec::new();
            let mut repo = None;
            for id in &ids {
                let task = plan.tasks.iter().find(|task| &task.id == id).ok_or_else(|| format!("No task {id}"))?;
                if id == plan::REVIEW_TASK || tasks.iter().any(|t: &Task| t.id == *id) {
                    return Err("Select each coding task once; the review task cannot be landed.".into());
                }
                let project = self.task_repo(index, task).ok_or("Task has no repository")?;
                if repo.as_ref().is_some_and(|repo| repo != &project) {
                    return Err("A landing must use tasks from one repository.".into());
                }
                repo = Some(project);
                tasks.push(task.clone());
            }
            let repo = repo.unwrap();
            let config = self.current_project_config(&repo);
            if config.land.target.trim().is_empty() || config.land.verify.trim().is_empty() {
                return Err("Configure [land] target and verify in the main, host or workspace config. Repository test_commands are suggestions only and are never executed by landing.".into());
            }
            if config.land.target == plan.integration_branch() {
                return Err("The landing target must differ from the integration branch.".into());
            }
            // Include every task whose code may already be on the integration branch. Lost or
            // closed sessions must never silently turn sandboxed verification into a host build.
            let sandboxed = config.isolation.required
                || plan.tasks.iter().filter(|t| t.id != plan::REVIEW_TASK).any(|task| {
                    task.session.as_ref().is_some_and(|id| {
                        self.sessions.iter().any(|s| &s.id == id && s.sandbox.is_some())
                            || self.store.sessions.iter().any(|s| {
                                (s.pane.as_ref() == Some(id) || task.worktree.as_ref() == Some(&s.cwd))
                                    && s.sandbox.is_some()
                            })
                    }) || config
                        .agents
                        .iter()
                        .find(|agent| agent.name == task.agent)
                        .map(|agent| config.isolation.mode_for(agent, task.isolation))
                        .unwrap_or(task.isolation.unwrap_or(config.isolation.mode))
                        == atomics_core::isolation::IsolationMode::Sandbox
                });
            let home = paths::atomics_home();
            let sandbox = if sandboxed {
                Some(crate::sandbox::Prepare {
                    home: home.clone(),
                    sbx: resolve_program(&self.sbx),
                    name: format!("atomics-land-{}", uuid::Uuid::new_v4().simple()),
                    agent: "shell",
                    cwd: PathBuf::new(),
                    config: config.isolation.sandbox.with_defaults(&config.resources, self.governor.cores),
                    warm: Vec::new(),
                    cache: config.cache.clone(),
                    exe: crate::setup::executable_path().map_err(|e| e.to_string())?,
                    socket: paths::socket_path(),
                    briefs: None,
                    pins: paths::git_pins_dir(),
                    relay: None,
                    secret_workspace: None,
                    github_account: None,
                })
            } else {
                None
            };
            Ok((repo, tasks, config.land, sandbox, home))
        })();
        let (repo, tasks, config, sandbox, home) = match setup {
            Ok(setup) => setup,
            Err(error) => {
                let _ = reply.send(ControlOutcome::Done(ControlResponse::error(error)));
                return;
            }
        };
        let (plan, into) = (self.plans[index].id.clone(), self.plans[index].integration_branch());
        let (actions, governor) = (self.action_tx.clone(), self.governor.clone());
        let started = json!({"plan":plan, "running":true, "target":config.target, "tasks":ids});
        self.landing.insert(plan.clone(), started.clone());
        self.integrating.insert((plan.clone(), String::new()));
        let _ = reply.send(ControlOutcome::Done(ControlResponse::ok(started)));
        std::thread::spawn(move || {
            let mut merged = Vec::new();
            let mut conflicts = Vec::new();
            let mut refused = Vec::new();
            let result = (|| -> anyhow::Result<Value> {
                let target = format!("refs/heads/{}", config.target);
                ensure!(
                    !config.target.starts_with('-')
                        && git::git(&repo, &["check-ref-format", "--branch", &config.target]).is_ok(),
                    "invalid landing target"
                );
                let before =
                    git::commit_of(&repo, &target)?.context("landing target must be an existing local branch")?;
                let mut worktree = None;
                for task in &tasks {
                    let (tx, rx) = std::sync::mpsc::channel();
                    let (plan, id) = (plan.clone(), task.id.clone());
                    actions
                        .send(Box::new(move |state| {
                            if let Some(index) = state.plans.iter().position(|p| p.id == plan) {
                                state.integrate_requested(index, &id, None, override_busy, true, Some(tx));
                            }
                        }))
                        .map_err(|_| anyhow::anyhow!("host stopped before integration"))?;
                    let ControlOutcome::Done(done) = rx.recv().context("integration did not answer")? else {
                        anyhow::bail!("unexpected integration response")
                    };
                    if done.ok {
                        worktree = done.result["worktree"].as_str().map(PathBuf::from);
                        merged.push(task.id.clone());
                    } else if let Some(files) = done.result["conflicts"].as_array() {
                        let names: Vec<_> = files.iter().filter_map(Value::as_str).map(String::from).collect();
                        let message = conflict_message(&task.id, &into, &names);
                        conflicts.push(
                            json!({"task":task.id,"branch":task.branch,"files":names,"message":message,"routed":false}),
                        );
                    } else {
                        refused.push(json!({"task":task.id,"error":done.error}));
                    }
                }
                let worktree = worktree.context("no selected task could be integrated; target left unchanged")?;
                let commit =
                    git::commit_of(&repo, &format!("refs/heads/{into}"))?.context("integration branch disappeared")?;
                ensure!(
                    git::is_ancestor(&repo, &before, &commit)? == Some(true),
                    "target is ahead or diverged; merge it into the integration branch and land again"
                );
                ensure!(
                    git::git(&worktree, &["status", "--porcelain"])?.is_empty(),
                    "integration worktree is dirty; target left unchanged"
                );
                let verified = verify_landing(&repo, &into, &commit, &config.verify, sandbox, &home, &governor)?;
                if verified.exit_code != Some(0) || verified.timed_out {
                    return Ok(json!({"landed":false,"target":config.target,"commit":commit,"verify":verified}));
                }
                ensure!(
                    git::commit_of(&repo, &format!("refs/heads/{into}"))?.as_deref() == Some(&commit),
                    "integration branch moved during verification; target left unchanged"
                );
                let (allowed, receiver) = std::sync::mpsc::channel();
                let (plan_id, branch) = (plan.clone(), into.clone());
                actions
                    .send(Box::new(move |state| {
                        let ready = !state.restarting
                            && state.tickets.workspace_identity_error.is_none()
                            && state.plans.iter().any(|p| {
                                p.id == plan_id
                                    && p.approved
                                    && p.status != PlanStatus::Archived
                                    && p.integration_branch() == branch
                            });
                        let _ = allowed.send(ready);
                    }))
                    .map_err(|_| anyhow::anyhow!("host stopped before landing"))?;
                ensure!(receiver.recv().unwrap_or(false), "plan changed or host is stopping; target left unchanged");
                {
                    let _write = crate::ref_guard::host_write(&repo, [&config.target]);
                    git::land_fast_forward(&repo, &config.target, &before, &commit)?;
                }
                let hook = if config.post_land.trim().is_empty() {
                    None
                } else {
                    // This user-owned hook runs only after the target advanced. A hook failure is
                    // reported separately: rolling back could overwrite work done since landing.
                    Some(match crate::sandbox::exec::run_bare(&repo, &config.post_land, &governor) {
                        Ok(executed) => json!(executed),
                        Err(error) => json!({"error":format!("{error:#}")}),
                    })
                };
                Ok(json!({"landed":true,"target":config.target,"commit":commit,"verify":verified,"post_land":hook}))
            })();
            let mut result = match result {
                Ok(value) => value,
                Err(error) => json!({"landed":false,"error":format!("{error:#}")}),
            };
            result["running"] = false.into();
            result["plan"] = plan.clone().into();
            result["merged"] = json!(merged);
            result["conflicts"] = json!(conflicts);
            result["refused"] = json!(refused);
            let _ = actions.send(Box::new(move |state| {
                if let Some(index) = state.plans.iter().position(|p| p.id == plan) {
                    if route {
                        for conflict in result["conflicts"].as_array_mut().into_iter().flatten() {
                            let id = conflict["task"].as_str().unwrap_or_default();
                            let session =
                                state.plans[index].tasks.iter().find(|t| t.id == id).and_then(|t| t.session.clone());
                            if let Some(session) = session.and_then(|id| state.sessions.iter().position(|s| s.id == id))
                            {
                                let delivery = state.deliver(session, conflict["message"].as_str().unwrap_or_default());
                                conflict["delivery"] = json!(format!("{delivery:?}"));
                                conflict["routed"] = (delivery != Delivery::Unavailable).into();
                            }
                        }
                    }
                    let landed = result["landed"] == true;
                    state.integrating.remove(&(plan.clone(), String::new()));
                    state.landing.insert(plan, result);
                    state.plan_event(index, "land_finished");
                    state.toast(if landed {
                        "Landing passed; target fast-forwarded."
                    } else {
                        "Landing did not advance the target; read plan_land status for details."
                    });
                }
            }));
        });
    }
}

fn conflict_message(task: &str, into: &str, files: &[String]) -> String {
    format!(
        "Landing task {task} into {into} conflicted in: {}. The merge was aborted. Merge {into} into your task branch, resolve these files, run the task's checks, commit the resolution, and report again. Keep the integration and target branches unchanged.",
        serde_json::to_string(files).unwrap_or_default()
    )
}

fn verify_landing(
    repo: &Path,
    branch: &str,
    commit: &str,
    command: &str,
    sandbox: Option<crate::sandbox::Prepare>,
    home: &Path,
    governor: &Governor,
) -> anyhow::Result<crate::sandbox::exec::Executed> {
    let parent = home.join("landing");
    std::fs::create_dir_all(&parent)?;
    let scratch = tempfile::Builder::new().prefix("verify-").tempdir_in(&parent)?;
    let checkout = scratch.path().join("checkout");
    // A separate object store and no writable link back to the source repository. A sandbox
    // cannot change the target by editing verification's refs, index or common directory.
    let bundle = scratch.path().join("source.bundle");
    git::git(repo, &["bundle", "create", &bundle.to_string_lossy(), &format!("refs/heads/{branch}")])?;
    git::git(
        scratch.path(),
        &["clone", "--no-checkout", "--", &bundle.to_string_lossy(), &checkout.to_string_lossy()],
    )?;
    git::git(&checkout, &["checkout", "--detach", commit])?;
    let result = match sandbox {
        None => crate::sandbox::exec::run_bare(&checkout, command, governor),
        Some(mut job) => {
            job.cwd = checkout.clone();
            let result = crate::sandbox::prepare(&job).and_then(|_| {
                crate::sandbox::exec::Exec {
                    sbx: job.sbx.clone(),
                    sandbox: job.name.clone(),
                    workdir: checkout.clone(),
                    command: command.into(),
                    env: crate::sandbox::cache_env(&job),
                    timeout: crate::sandbox::exec::timeout(None),
                    class: SlotClass::Test,
                    ancestors: Vec::new(),
                    owner: "host".into(),
                }
                .run(governor, || false)
            });
            // Keep the scratch checkout if teardown fails; never remove files still mounted in a VM.
            if let Err(error) = crate::sandbox::remove(&job.home, &job.sbx, &job.name) {
                let kept = scratch.keep();
                anyhow::bail!(
                    "verification sandbox cleanup failed: {error:#}; checkout retained at {}",
                    kept.display()
                );
            }
            result
        }
    }?;
    if result.exit_code != Some(0) || result.timed_out {
        return Ok(result);
    }
    ensure!(
        git::commit_of(&checkout, "HEAD")?.as_deref() == Some(commit),
        "verification changed its HEAD; target left unchanged"
    );
    ensure!(
        git::git(&checkout, &["diff", "--name-only", "HEAD"])?.is_empty(),
        "verification modified tracked files; target left unchanged"
    );
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::super::sandbox_tests::tick_until;
    use super::super::tests::{repository, request, session, with_app};
    use super::*;

    fn task(repo: &Path, id: &str, file: &str, contents: &str) -> Task {
        let tree = repo.with_file_name(id);
        git::add_worktree(repo, &tree, id, "main").unwrap();
        let start = git::commit_of(&tree, "HEAD").unwrap();
        std::fs::write(tree.join(file), contents).unwrap();
        git::commit_all(&tree, "Task change").unwrap();
        Task {
            id: id.into(),
            title: id.into(),
            agent: "test".into(),
            status: TaskStatus::Review,
            repo: Some(repo.display().to_string()),
            branch: Some(id.into()),
            worktree: Some(tree),
            start_commit: start,
            isolation: Some(atomics_core::isolation::IsolationMode::Bare),
            ..Default::default()
        }
    }

    fn setup(app: &mut Desktop<'_>, repo: &Path, tasks: Vec<Task>, verify: &str) {
        app.runtime.config.base_ref = "main".into();
        app.runtime.config.isolation.mode = atomics_core::isolation::IsolationMode::Bare;
        app.runtime.config.land =
            atomics_core::config::LandConfig { target: "main".into(), verify: verify.into(), post_land: String::new() };
        app.runtime.add_project(repo);
        app.runtime.plans.push(Plan {
            id: "landing-test".into(),
            project: Some(repo.into()),
            approved: true,
            tasks,
            ..Default::default()
        });
        app.runtime.selected_plan = app.runtime.plans.len() - 1;
    }

    fn land(app: &mut Desktop<'_>, ctx: &egui::Context, ids: &[&str], route: bool) -> Value {
        let response =
            request(app, json!({"cmd":"plan_land", "plan":"landing-test", "tasks":ids, "route_conflicts":route}));
        assert!(response.ok, "{response:?}");
        assert_eq!(response.result["running"], true);
        tick_until(app, ctx, "landing result", |app| app.runtime.landing["landing-test"]["running"] == false);
        request(app, json!({"cmd":"plan_land", "plan":"landing-test", "status":true})).result
    }

    #[test]
    fn landing_passes_fast_forwards_and_runs_post_land_hook() {
        with_app("app::landing::tests::landing_passes_fast_forwards_and_runs_post_land_hook", |app, ctx| {
            let (_dir, repo) = repository();
            let task = task(&repo, "feature", "feature.txt", "ready");
            setup(app, &repo, vec![task], "test -f feature.txt && echo passed");
            app.runtime.config.land.post_land = "printf hooked > hook-result".into();
            let result = land(app, ctx, &["feature"], false);
            assert_eq!(result["landed"], true, "{result}");
            assert_eq!(result["verify"]["exit_code"], 0);
            assert_eq!(result["post_land"]["exit_code"], 0);
            assert_eq!(git::commit_of(&repo, "HEAD").unwrap().as_deref(), result["commit"].as_str());
            assert!(repo.join("feature.txt").exists());
            assert_eq!(std::fs::read_to_string(repo.join("hook-result")).unwrap(), "hooked");
            assert!(app.runtime.events.iter().any(|event| event.change == Some("land_finished")));
        });
    }

    #[test]
    fn landing_completed_plan_and_hook_failure_keep_the_successful_target() {
        with_app(
            "app::landing::tests::landing_completed_plan_and_hook_failure_keep_the_successful_target",
            |app, ctx| {
                let (_dir, repo) = repository();
                let task = task(&repo, "feature", "feature.txt", "ready");
                setup(app, &repo, vec![task], "true");
                app.runtime.plans[app.runtime.selected_plan].status = PlanStatus::Completed;
                app.runtime.config.cleanup.completed_plans = false;
                app.runtime.config.land.post_land = "echo 'preview failed' >&2; exit 9".into();
                let result = land(app, ctx, &["feature"], false);
                assert_eq!(result["landed"], true, "{result}");
                assert_eq!(result["post_land"]["exit_code"], 9);
                assert_eq!(git::commit_of(&repo, "HEAD").unwrap().as_deref(), result["commit"].as_str());
            },
        );
    }

    #[test]
    fn landing_failure_reports_tests_and_preserves_target_and_skips_hook() {
        with_app(
            "app::landing::tests::landing_failure_reports_tests_and_preserves_target_and_skips_hook",
            |app, ctx| {
                let (_dir, repo) = repository();
                let before = git::commit_of(&repo, "HEAD").unwrap();
                let task = task(&repo, "feature", "feature.txt", "broken");
                setup(app, &repo, vec![task], "echo 'FAILED feature_contract' >&2; exit 7");
                app.runtime.config.land.post_land = "touch should-not-run".into();
                let result = land(app, ctx, &["feature"], false);
                assert_eq!(result["landed"], false, "{result}");
                assert_eq!(result["verify"]["exit_code"], 7, "{result}");
                assert!(result["verify"]["stderr"].as_str().unwrap().contains("FAILED feature_contract"));
                assert_eq!(git::commit_of(&repo, "HEAD").unwrap(), before);
                assert!(!repo.join("should-not-run").exists());
            },
        );
    }

    #[test]
    fn landing_batch_continues_after_conflict_and_routes_files_to_owner() {
        with_app(
            "app::landing::tests::landing_batch_continues_after_conflict_and_routes_files_to_owner",
            |app, ctx| {
                let (_dir, repo) = repository();
                std::fs::write(repo.join("shared.txt"), "base\n").unwrap();
                git::commit_all(&repo, "Shared base").unwrap();
                let first = task(&repo, "first", "shared.txt", "first\n");
                let mut conflict = task(&repo, "conflict", "shared.txt", "second\n");
                conflict.session = Some("owner".into());
                app.runtime.sessions.push(session(
                    "owner",
                    AgentKind::Generic,
                    AgentStatus::Idle,
                    conflict.worktree.as_deref().unwrap(),
                ));
                let last = task(&repo, "last", "last.txt", "last\n");
                setup(app, &repo, vec![first, conflict, last], "test -f last.txt");
                let result = land(app, ctx, &["first", "conflict", "last"], true);
                assert_eq!(result["landed"], true, "{result}");
                assert_eq!(result["merged"], json!(["first", "last"]));
                assert_eq!(result["conflicts"][0]["files"], json!(["shared.txt"]));
                assert_eq!(result["conflicts"][0]["routed"], true);
                let messages = app.runtime.hub.lock().unwrap().outbox["owner"].clone();
                assert!(messages[0].contains("shared.txt") && messages[0].contains("Merge atomics/plan-landing-test"));
                assert_eq!(std::fs::read_to_string(repo.join("shared.txt")).unwrap(), "first\n");
                assert!(repo.join("last.txt").exists());
                assert_eq!(app.runtime.plans[app.runtime.selected_plan].tasks[1].status, TaskStatus::Review);
            },
        );
    }

    #[test]
    fn landing_waits_for_test_slot_and_sandbox_tasks_use_sandbox_exec() {
        with_app("app::landing::tests::landing_waits_for_test_slot_and_sandbox_tasks_use_sandbox_exec", |app, ctx| {
            let (_dir, repo) = repository();
            let mut task = task(&repo, "feature", "feature.txt", "ready");
            task.isolation = Some(atomics_core::isolation::IsolationMode::Sandbox);
            setup(app, &repo, vec![task], "test -f feature.txt && echo sandbox-verified");
            let bin = crate::test_support::tempdir().unwrap();
            app.runtime.sbx = crate::sandbox::fake::install(bin.path()).display().to_string();
            std::fs::write(bin.path().join("exec-runs"), "").unwrap();
            app.runtime
                .governor
                .set_config(atomics_core::governor::ResourceConfig { test_slots: Some(1), ..Default::default() });
            let governor = app.runtime.governor.clone();
            let held = governor.acquire(SlotClass::Test, "other test".into(), Vec::new(), |_| Ok(())).unwrap();
            let before = git::commit_of(&repo, "HEAD").unwrap();
            assert!(request(app, json!({"cmd":"plan_land","plan":"landing-test","tasks":["feature"]})).ok);
            tick_until(app, ctx, "landing waits for slot", |_| !governor.snapshot().1.is_empty());
            assert_eq!(git::commit_of(&repo, "HEAD").unwrap(), before);
            assert!(
                crate::sandbox::fake::calls_to(bin.path(), &["exec"])
                    .iter()
                    .all(|args| !args.iter().any(|arg| arg.contains("sandbox-verified")))
            );
            drop(held);
            tick_until(app, ctx, "sandbox landing result", |app| {
                app.runtime.landing["landing-test"]["running"] == false
            });
            let result = &app.runtime.landing["landing-test"];
            assert_eq!(result["landed"], true, "{result}");
            assert!(result["verify"]["stdout"].as_str().unwrap().contains("sandbox-verified"));
            assert!(
                crate::sandbox::fake::calls_to(bin.path(), &["exec"])
                    .iter()
                    .any(|args| args.iter().any(|arg| arg.contains("sandbox-verified")))
            );
        });
    }

    #[test]
    fn integrate_control_refuses_working_and_reported_then_changed_tasks() {
        with_app(
            "app::landing::tests::integrate_control_refuses_working_and_reported_then_changed_tasks",
            |app, ctx| {
                let (_dir, repo) = repository();
                let mut task = task(&repo, "feature", "feature.txt", "ready");
                let tree = task.worktree.clone().unwrap();
                task.session = Some("worker".into());
                app.runtime.sessions.push(session("worker", AgentKind::Generic, AgentStatus::Working, &tree));
                setup(app, &repo, vec![task], "true");
                let integrate = |app: &mut Desktop<'_>, value| {
                    let (tx, rx) = channel();
                    app.runtime.control(serde_json::from_value(value).unwrap(), tx);
                    let mut answer = None;
                    tick_until(app, ctx, "integrate answer", |_| {
                        if let Ok(ControlOutcome::Done(done)) = rx.try_recv() {
                            answer = Some(done);
                        }
                        answer.is_some()
                    });
                    answer.unwrap()
                };
                let cmd = json!({"cmd":"plan_integrate","plan":"landing-test","id":"feature"});
                let result = integrate(app, cmd.clone());
                assert!(!result.ok && result.error.unwrap().contains("working"));
                app.runtime.sessions[0].status = AgentStatus::Idle;
                app.runtime.report(Some("worker"), "done", "milestone ready");
                std::thread::sleep(Duration::from_millis(10));
                std::fs::write(tree.join("unfinished.txt"), "later edits").unwrap();
                let result = integrate(app, cmd.clone());
                assert!(!result.ok && result.error.unwrap().contains("since"));
                let mut milestone = cmd;
                milestone["commit"] = git::commit_of(&tree, "HEAD").unwrap().unwrap().into();
                assert!(integrate(app, milestone).ok);
                assert!(git::git(&tree, &["status", "--porcelain"]).unwrap().contains("unfinished.txt"));
            },
        );
    }
}
