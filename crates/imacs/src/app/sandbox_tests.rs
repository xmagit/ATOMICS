//! Agents in Docker Sandboxes, against the fake `sbx` in tests/fixtures.
use super::tests::{repository, request, with_app};
use super::*;
use crate::sandbox::fake;
use imacs_core::isolation::IsolationMode;

#[test]
fn startup_cleanup_does_not_block_the_runtime_while_sbx_is_waiting() {
    with_app("app::sandbox_tests::startup_cleanup_does_not_block_the_runtime_while_sbx_is_waiting", |app, ctx| {
        if let Some(job) = app.runtime.startup_cleanup.take() {
            job.join().unwrap();
        }
        let (_dir, repo) = repository();
        let bin = crate::test_support::tempdir().unwrap();
        app.runtime.sbx = fake::install(bin.path()).display().to_string();
        let job = crate::sandbox::Prepare {
            home: paths::imacs_home(),
            sbx: app.runtime.sbx.clone(),
            name: "imacs-orphan".into(),
            agent: "shell",
            cwd: repo,
            config: Default::default(),
            warm: Vec::new(),
            pins: paths::git_pins_dir(),
            exe: "/imacs".into(),
            socket: paths::socket_path(),
            briefs: None,
        };
        crate::sandbox::prepare(&job).unwrap();
        let gate = bin.path().join("wait-ls");
        std::fs::write(&gate, "wait").unwrap();
        app.runtime.begin_startup_cleanup();
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(60));
        while !bin.path().join("waiting-ls").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let reached_sbx = bin.path().join("waiting-ls").exists();
        app.runtime.runtime_tick(ctx); // Must return while the sweep is blocked in sbx.
        let still_waiting = app.runtime.startup_cleanup.as_ref().is_some_and(|job| !job.is_finished());
        std::fs::remove_file(gate).unwrap();
        app.runtime.startup_cleanup.take().unwrap().join().unwrap();
        assert!(reached_sbx && still_waiting);
        assert!(!bin.path().join("sandboxes/imacs-orphan").exists());
        assert!(!bin.path().join("mcp/imacs-orphan").exists());
    });
}

#[test]
fn archiving_during_worktree_creation_does_not_launch_the_agent() {
    with_app("app::sandbox_tests::archiving_during_worktree_creation_does_not_launch_the_agent", |app, ctx| {
        let (dir, repo) = repository();
        let marker = dir.path().join("agent-ran");
        app.runtime.config.agents.push(probe("probe", &marker, Some(IsolationMode::Bare)));
        app.runtime.plans = vec![Plan {
            id: "late".into(),
            project: Some(repo.clone()),
            approved: true,
            tasks: vec![Task { id: "a".into(), agent: "probe".into(), ..Task::default() }],
            ..Plan::default()
        }];
        app.runtime.dispatch(0, Some("a")).unwrap();
        app.runtime.close_plan(0, PlanStatus::Archived).unwrap();
        tick_until(app, ctx, "the closed plan's worktree job", |app| app.runtime.creating.is_empty());
        app.runtime.finish_lifecycle_jobs();
        assert!(!marker.exists());
        let task = &app.runtime.plans[0].tasks[0];
        assert!(!task.worktree.as_ref().unwrap().exists());
        assert!(
            git::git(&repo, &["show-ref", "--verify", &format!("refs/heads/{}", task.branch.as_ref().unwrap())])
                .is_ok()
        );
    });
}

#[test]
fn quitting_stops_running_and_preparing_sandboxes_and_joins_cleanup() {
    with_app("app::sandbox_tests::quitting_stops_running_and_preparing_sandboxes_and_joins_cleanup", |app, ctx| {
        let (_dir, repo) = repository();
        let bin = crate::test_support::tempdir().unwrap();
        app.runtime.sbx = fake::install(bin.path()).display().to_string();
        std::fs::write(bin.path().join("run-waits"), "wait").unwrap();
        app.runtime.config.isolation.mode = IsolationMode::Sandbox;
        // The default claude agent is bare; this test sandboxes it.
        app.runtime.config.agents.iter_mut().for_each(|agent| agent.isolation = None);
        let running = app.runtime.start(ctx, "claude", repo.clone(), None, vec![], None).unwrap();
        tick_until(app, ctx, "running sandbox", |app| {
            app.runtime.sessions.iter().any(|s| s.id == running && s.terminal.is_some())
        });
        let preparing = app.runtime.start(ctx, "claude", repo, None, vec![], None).unwrap();
        assert!(!app.runtime.pending_sandbox.is_empty());
        app.runtime.shutdown();
        let stopped = fake::calls_to(bin.path(), &["stop"]);
        for id in [&running, &preparing] {
            let name = format!("imacs-{id}");
            assert!(stopped.iter().any(|call| call == &["stop", name.as_str()]));
            assert!(bin.path().join("sandboxes").join(name).exists(), "quit preserves resumability");
        }
        assert!(app.runtime.pending_sandbox.is_empty() && app.runtime.lifecycle_jobs.is_empty());
        assert!(
            app.runtime
                .sessions
                .iter()
                .filter_map(|s| s.terminal.as_ref())
                .all(|t| unsafe { libc::kill(-(t.process_id() as i32), 0) } != 0)
        );
        app.runtime.sessions.clear();
    });
}

#[test]
fn plan_close_cleanup_settings_control_worktrees_and_archive_keeps_branches() {
    with_app(
        "app::sandbox_tests::plan_close_cleanup_settings_control_worktrees_and_archive_keeps_branches",
        |app, _ctx| {
            let (dir, repo) = repository();
            for (status, enabled, name) in [
                (PlanStatus::Completed, true, "complete"),
                (PlanStatus::Archived, true, "archive"),
                (PlanStatus::Completed, false, "disabled"),
                (PlanStatus::Archived, false, "archive-disabled"),
            ] {
                let path = dir.path().join(name);
                git::add_worktree(&repo, &path, name, "main").unwrap();
                let plan = Plan {
                    id: name.into(),
                    project: Some(repo.clone()),
                    tasks: vec![Task {
                        id: "a".into(),
                        status: TaskStatus::Done,
                        branch: Some(name.into()),
                        worktree: Some(path.clone()),
                        ..Task::default()
                    }],
                    ..Plan::default()
                };
                git::git(&repo, &["branch", &plan.integration_branch()]).unwrap();
                app.runtime.plans.push(plan);
                app.runtime.config.cleanup.completed_plans = enabled;
                app.runtime.config.cleanup.archived_plans = enabled;
                let index = app.runtime.plans.len() - 1;
                app.runtime.close_plan(index, status).unwrap();
                app.runtime.finish_lifecycle_jobs();
                assert_eq!(path.exists(), !enabled);
                let exists = git::git(&repo, &["show-ref", "--verify", &format!("refs/heads/{name}")]).is_ok();
                assert_eq!(exists, !enabled || status == PlanStatus::Archived);
            }
        },
    );
}

/// Bound a hang without assuming a parallel, lower-priority test gets CPU within ten seconds.
fn tick_until(app: &mut Desktop<'_>, ctx: &egui::Context, what: &str, mut done: impl FnMut(&mut Desktop<'_>) -> bool) {
    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(60));
    while !done(app) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        app.runtime.runtime_tick(ctx);
        if !done(app) {
            // Preparation, terminal output/exit and cleanup completions wake the host. The
            // short fallback also notices fixture files written without a terminal event.
            app.runtime.wake.wait_until(deadline.min(Instant::now() + Duration::from_millis(100)));
        }
    }
}

/// A test must finish preparation and stop jobs before dropping the directory containing sbx.
fn finish_agents(app: &mut Desktop<'_>, ctx: &egui::Context) {
    let ids: Vec<_> = app.runtime.sessions.iter().map(|s| s.id.clone()).collect();
    for id in ids {
        app.runtime.close(&id);
    }
    tick_until(app, ctx, "pending sandbox preparation to finish", |app| app.runtime.pending_sandbox.is_empty());
    app.runtime.finish_lifecycle_jobs();
}

fn worktree(repo: &Path, name: &str) -> PathBuf {
    let path = repo.parent().unwrap().join(format!("repo-{name}"));
    git::add_worktree(repo, &path, name, "main").unwrap();
    path
}

fn probe(name: &str, out: &Path, isolation: Option<IsolationMode>) -> imacs_core::config::AgentConfig {
    imacs_core::config::AgentConfig {
        name: name.into(),
        command: vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("pwd > {0}.$$.tmp; mv {0}.$$.tmp {0}; printf 'probe ready\\n'; exec sleep 600", out.display()),
        ],
        kind: AgentKind::Generic,
        busy_text: None,
        isolation,
    }
}

#[test]
fn sandboxed_agents_attach_through_sbx_run_and_reattach_after_a_restart() {
    with_app("app::sandbox_tests::sandboxed_agents_attach_through_sbx_run_and_reattach_after_a_restart", |app, ctx| {
        let (_dir, repo) = repository();
        let worktree = worktree(&repo, "task");
        let bin = crate::test_support::tempdir().unwrap();
        app.runtime.sbx = fake::install(bin.path()).display().to_string();
        app.runtime.config.isolation.mode = IsolationMode::Sandbox;
        // The default claude agent is bare; this test sandboxes it.
        app.runtime.config.agents.iter_mut().for_each(|agent| agent.isolation = None);

        let id = app.runtime.start(ctx, "claude", worktree.clone(), None, vec![], Some("Fix the bug".into())).unwrap();
        let name = format!("imacs-{id}");
        let index = app.runtime.find(&id).unwrap();
        assert!(app.runtime.sessions[index].terminal.is_none(), "the tile waits for its sandbox");
        tick_until(app, ctx, "the agent to attach", |_| bin.path().join("run-ssh").exists());

        let create = &fake::calls_to(bin.path(), &["create"])[0];
        assert_eq!(create[..3], ["create", "--name", name.as_str()]);
        // Without settings, a sandbox gets 8g and one test run's share of the job pool.
        let resources = &app.runtime.config.resources;
        let cpus = (resources.jobs(app.runtime.governor.cores)
            / resources.slots(imacs_core::governor::SlotClass::Test, app.runtime.governor.cores))
        .max(2);
        assert_eq!(create[4..8], ["--memory", "8g", "--cpus", cpus.to_string().as_str()], "{create:?}");
        let agent = create.iter().position(|arg| arg == "claude").unwrap();
        assert_eq!(Path::new(&create[agent + 1]), worktree.canonicalize().unwrap());
        let run = &fake::calls_to(bin.path(), &["run"])[0];
        assert_eq!(run[..5], ["run", "--name", name.as_str(), "claude", "--"]);
        let session_id = app.runtime.sessions[index].claude_id.clone().unwrap();
        // Its status line only saves the account's limits in the VM; host hooks and MCP config stay out.
        let settings = imacs_core::isolation::claude_settings().to_string();
        assert_eq!(run[5..], ["--settings", settings.as_str(), "--session-id", session_id.as_str(), "Fix the bug"]);
        assert!(!run.iter().any(|arg| arg == "--mcp-config" || arg.contains("hook")), "host hooks stay out: {run:?}");
        assert_eq!(std::fs::read_to_string(bin.path().join("run-ssh")).unwrap().trim(), "unset");
        assert_eq!(app.runtime.sessions[index].sandbox.as_deref(), Some(name.as_str()));
        assert_eq!(app.runtime.pane_session(Some(name.clone())), Some(id.clone()), "reports find their session");
        let record = app.runtime.store.sessions.iter().find(|r| r.claude_session_id == session_id).unwrap().clone();
        assert_eq!(record.sandbox.as_deref(), Some(name.as_str()));

        // The agent exits, and its sandbox stops until the next `sbx run`.
        tick_until(app, ctx, "the sandbox to stop", |_| !fake::calls_to(bin.path(), &["stop"]).is_empty());
        assert_eq!(fake::calls_to(bin.path(), &["stop"]), [["stop", name.as_str()]]);

        // A restart resumes the session in the same sandbox, without making another.
        finish_agents(app, ctx);
        std::fs::remove_file(bin.path().join("run-ssh")).unwrap();
        app.runtime.resume_record(ctx, record);
        tick_until(app, ctx, "the resumed agent to attach", |_| bin.path().join("run-ssh").exists());
        assert_eq!(fake::calls_to(bin.path(), &["create"]).len(), 1);
        let run = fake::calls_to(bin.path(), &["run"]).pop().unwrap();
        assert_eq!(
            run,
            [
                "run",
                "--name",
                name.as_str(),
                "claude",
                "--",
                "--settings",
                settings.as_str(),
                "--resume",
                session_id.as_str()
            ]
        );
        assert_eq!(app.runtime.sessions[0].sandbox.as_deref(), Some(name.as_str()));
        finish_agents(app, ctx);
    });
}

#[test]
fn chief_checks_run_in_the_session_sandbox_and_bare_sessions_are_refused() {
    with_app(
        "app::sandbox_tests::chief_checks_run_in_the_session_sandbox_and_bare_sessions_are_refused",
        |app, ctx| {
            let (_dir, repo) = repository();
            let worktree = worktree(&repo, "task");
            let bin = crate::test_support::tempdir().unwrap();
            app.runtime.sbx = fake::install(bin.path()).display().to_string();
            std::fs::create_dir(bin.path().join("sandboxes")).unwrap();
            std::fs::write(bin.path().join("sandboxes/imacs-boxed"), r#"{"name":"imacs-boxed","status":"running"}"#)
                .unwrap();
            let home = paths::imacs_home();
            crate::sandbox_ownership::created(&home, "imacs-boxed", crate::sandbox_ownership::Resource::Sandbox)
                .unwrap();
            crate::sandbox_ownership::created(&home, "imacs-boxed", crate::sandbox_ownership::Resource::Mcp).unwrap();
            std::fs::create_dir_all(bin.path().join("mcp")).unwrap();
            std::fs::write(bin.path().join("mcp/imacs-boxed"), "registration").unwrap();
            let owned = std::fs::read(home.join("sandbox-ownership.json")).unwrap();
            let session = |id: &str| super::tests::session(id, AgentKind::Claude, AgentStatus::Done, &worktree);
            let mut boxed = session("boxed");
            boxed.sandbox = Some("imacs-boxed".into());
            boxed.terminal = Some(
                Terminal::spawn(
                    app.runtime.wake.clone(),
                    app.runtime.palette.terminal_colors(),
                    "/bin/sh".into(),
                    vec!["-c".into(), "exec sleep 30".into()],
                    &worktree,
                    HashMap::new(),
                    100,
                )
                .unwrap(),
            );
            let pid = boxed.terminal.as_ref().unwrap().process_id();
            let mut chief = session("chief");
            chief.chief = true;
            app.runtime.sessions = vec![session("bare"), boxed, chief];

            // Bare sessions and the chief work on this machine, where their checks run.
            for id in ["bare", "chief"] {
                let refused = request(app, json!({"cmd":"exec", "session":id, "command":"cargo test"}));
                let error = refused.error.unwrap();
                assert!(error.contains("runs bare") && error.contains("run the command on the host"), "{error}");
                assert!(error.contains(&worktree.display().to_string()) && error.contains("imacs queue --class test"));
            }
            let empty = request(app, json!({"cmd":"exec", "session":"boxed", "command":" "}));
            assert_eq!(empty.error.as_deref(), Some("give a command to run"));

            let exec = |app: &mut Desktop<'_>, request: serde_json::Value| {
                let (tx, rx) = channel();
                app.runtime.control(ctx, serde_json::from_value(request).unwrap(), tx);
                match rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                    ControlOutcome::Exec { job, session, then } => (job, session, then),
                    _ => panic!("a sandboxed session's command runs on the socket thread"),
                }
            };
            let (job, session, then) = exec(
                app,
                json!({"cmd":"exec", "session":"boxed", "command":"cargo test", "timeout_secs":9000, "class":"qa"}),
            );
            assert_eq!((job.sbx.as_str(), job.sandbox.as_str()), (app.runtime.sbx.as_str(), "imacs-boxed"));
            assert_eq!(job.workdir, worktree.canonicalize().unwrap(), "where the sandbox mounts the worktree");
            assert_eq!((job.command.as_str(), job.class), ("cargo test", imacs_core::governor::SlotClass::Qa));
            assert_eq!(job.timeout, Duration::from_secs(3600), "at most an hour");
            assert_eq!(job.ancestors, [pid], "the session's tile shows the slot");
            assert_eq!(session, json!("boxed"));
            // Its agent still runs in the sandbox, which stays up.
            then();
            app.runtime.runtime_tick(ctx);
            app.runtime.finish_lifecycle_jobs();
            assert!(fake::calls_to(bin.path(), &["stop"]).is_empty());

            // Once the agent has gone, a sandbox its check started stops again.
            let (job, _, then) = exec(app, json!({"cmd":"exec", "session":"boxed", "command":"cargo test"}));
            assert_eq!((job.timeout, job.class), (Duration::from_secs(600), imacs_core::governor::SlotClass::Test));
            app.runtime.sessions[1].terminal = None;
            then();
            tick_until(app, ctx, "the sandbox to stop", |_| !fake::calls_to(bin.path(), &["stop"]).is_empty());
            assert_eq!(fake::calls_to(bin.path(), &["stop"]), [["stop", "imacs-boxed"]]);
            app.runtime.finish_lifecycle_jobs();
            assert!(bin.path().join("sandboxes/imacs-boxed").exists(), "checks leave the sandbox resumable");
            assert!(bin.path().join("mcp/imacs-boxed").exists());
            assert_eq!(std::fs::read(home.join("sandbox-ownership.json")).unwrap(), owned);

            // A stale session or completion cannot stop another home's sandbox.
            let other_home = crate::test_support::tempdir().unwrap();
            crate::sandbox_ownership::created(
                other_home.path(),
                "imacs-foreign",
                crate::sandbox_ownership::Resource::Sandbox,
            )
            .unwrap();
            std::fs::write(
                bin.path().join("sandboxes/imacs-foreign"),
                r#"{"name":"imacs-foreign","status":"running"}"#,
            )
            .unwrap();
            app.runtime.exec_finished("gone", "imacs-foreign".into());
            app.runtime.finish_lifecycle_jobs();
            assert_eq!(fake::calls_to(bin.path(), &["stop"]), [["stop", "imacs-boxed"]]);
            assert!(std::fs::read_to_string(bin.path().join("sandboxes/imacs-foreign")).unwrap().contains("running"));
            app.runtime.sessions.clear();
        },
    );
}

#[test]
fn sandboxed_task_args_drop_host_sandbox_flags_and_bare_ones_keep_them() {
    with_app("app::sandbox_tests::sandboxed_task_args_drop_host_sandbox_flags_and_bare_ones_keep_them", |app, ctx| {
        let (_dir, repo) = repository();
        let worktree = worktree(&repo, "task");
        let bin = crate::test_support::tempdir().unwrap();
        app.runtime.sbx = fake::install(bin.path()).display().to_string();
        app.runtime.config.isolation.mode = IsolationMode::Sandbox;
        // The arguments IMACS plans give a Codex task, and an agent command carrying a host flag.
        std::fs::write(bin.path().join("wait-agent"), "wait").unwrap();
        let task_args: Vec<String> = [
            "-m",
            "gpt-6.1-sol",
            "-c",
            "model_reasoning_effort=high",
            "-s",
            "workspace-write",
            "-a",
            "on-request",
            "-c",
            r#"sandbox_workspace_write.writable_roots=["/Users/maxfalk/.cargo"]"#,
        ]
        .map(String::from)
        .into();
        let codex = app.runtime.config.agents.iter_mut().find(|a| a.name == "codex").unwrap();
        codex.command = vec!["codex".into(), "--full-auto".into(), "--search".into()];
        let sandbox_id = app
            .runtime
            .start(ctx, "codex", worktree.clone(), None, task_args.clone(), Some("Do the task".into()))
            .unwrap();
        tick_until(app, ctx, "the agent to reach its fixture hook", |_| bin.path().join("waiting-agent").exists());
        let run = fake::calls_to(bin.path(), &["run"]).pop().unwrap();
        let trusted =
            format!("projects={{ {:?} = {{ trust_level = \"trusted\" }} }}", worktree.canonicalize().unwrap());
        assert_eq!(
            run[4..],
            ["--", "--search", "-c", &trusted, "-m", "gpt-6.1-sol", "-c", "model_reasoning_effort=high", "Do the task"],
            "{run:?}"
        );

        // A bare agent gets every argument, as before.
        let out = bin.path().join("bare-args");
        // `agent` is the fake sbx's hook. Reusing that filename lets the still-starting
        // sandbox execute the bare probe and overwrite its arguments with sbx run's args.
        let script = bin.path().join("bare-agent.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > {}.$$.tmp\nmv {0}.$$.tmp {0}\n", out.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        app.runtime.config.agents.push(imacs_core::config::AgentConfig {
            name: "bare-agent".into(),
            command: vec![script.display().to_string()],
            kind: AgentKind::Generic,
            busy_text: None,
            isolation: Some(IsolationMode::Bare),
        });
        let bare_id = app.runtime.start(ctx, "bare-agent", worktree.clone(), None, task_args.clone(), None).unwrap();
        tick_until(app, ctx, "the bare agent to finish", |app| {
            app.runtime
                .sessions
                .iter()
                .find(|s| s.id == bare_id)
                .and_then(|s| s.terminal.as_ref())
                .is_some_and(|t| t.exited())
        });
        std::fs::remove_file(bin.path().join("wait-agent")).unwrap();
        tick_until(app, ctx, "the sandboxed agent to finish", |app| {
            app.runtime
                .sessions
                .iter()
                .find(|s| s.id == sandbox_id)
                .and_then(|s| s.terminal.as_ref())
                .is_some_and(|t| t.exited())
        });
        let given: Vec<String> = std::fs::read_to_string(&out).unwrap().lines().map(String::from).collect();
        assert!(given.ends_with(&task_args), "{given:?}");
        finish_agents(app, ctx);
    });
}

#[test]
fn bare_agents_never_touch_sbx_and_a_repository_can_only_tighten() {
    with_app("app::sandbox_tests::bare_agents_never_touch_sbx_and_a_repository_can_only_tighten", |app, ctx| {
        let (_dir, repo) = repository();
        let worktree = worktree(&repo, "task");
        let bin = crate::test_support::tempdir().unwrap();
        app.runtime.sbx = fake::install(bin.path()).display().to_string();
        let out = bin.path().join("probe-ran");
        app.runtime.config.agents.push(probe("probe", &out, None));
        app.runtime.config.agents.push(probe("bare-probe", &out, Some(IsolationMode::Bare)));

        // Bare mode, and an agent set bare under sandbox mode, start on this machine as before.
        for (mode, agent) in [(IsolationMode::Bare, "probe"), (IsolationMode::Sandbox, "bare-probe")] {
            app.runtime.config.isolation.mode = mode;
            let id = app.runtime.start(ctx, agent, worktree.clone(), None, vec![], None).unwrap();
            assert!(app.runtime.sessions[app.runtime.find(&id).unwrap()].terminal.is_some(), "{agent} starts at once");
            tick_until(app, ctx, "the bare probe", |_| out.exists());
            std::fs::remove_file(&out).unwrap();
            assert!(app.runtime.sessions[app.runtime.find(&id).unwrap()].sandbox.is_none());
        }
        assert!(fake::calls(bin.path()).is_empty(), "bare agents never call sbx");

        // An agent sbx cannot run is refused rather than run bare.
        app.runtime.config.isolation.mode = IsolationMode::Sandbox;
        let error = app.runtime.start(ctx, "probe", worktree.clone(), None, vec![], None).unwrap_err();
        assert!(error.contains("no agent for /bin/sh") && error.contains("isolation = \"bare\""), "{error}");

        // A repository that requires sandboxes overrides the user's bare mode and the agent's own.
        app.runtime.config.isolation.mode = IsolationMode::Bare;
        app.runtime.config.isolation.sandbox.template = Some("workspace:local".into());
        std::fs::create_dir(repo.join(".imacs")).unwrap();
        std::fs::write(
            repo.join(".imacs/config.toml"),
            "[isolation]\nmode = \"sandbox\"\n[isolation.sandbox]\ntemplate = \"repo:local\"\n",
        )
        .unwrap();
        std::fs::create_dir(worktree.join(".imacs")).unwrap();
        std::fs::write(worktree.join(".imacs/config.toml"), "[isolation.sandbox]\ntemplate = \"worktree:local\"\n")
            .unwrap();
        let error = app.runtime.start(ctx, "bare-probe", worktree.clone(), None, vec![], None).unwrap_err();
        assert!(error.contains("requires sandboxed agents"), "{error}");
        assert!(!out.exists());
        let id = app.runtime.start(ctx, "shell", worktree.clone(), None, vec![], None).unwrap();
        tick_until(app, ctx, "the sandboxed shell", |_| bin.path().join("run-ssh").exists());
        let run = fake::calls_to(bin.path(), &["run"]).pop().unwrap();
        assert_eq!(run, ["run", "--name", format!("imacs-{id}").as_str(), "shell", "--"]);
        let create = fake::calls_to(bin.path(), &["create"]).pop().unwrap();
        assert!(create.windows(2).any(|args| args == ["--template", "repo:local"]));

        // A repository asking for bare is ignored: sandbox mode stays.
        std::fs::write(repo.join(".imacs/config.toml"), "[isolation]\nmode = \"bare\"\n").unwrap();
        app.runtime.config.isolation.mode = IsolationMode::Sandbox;
        let id = app.runtime.start(ctx, "shell", worktree.clone(), None, vec![], None).unwrap();
        assert!(app.runtime.sessions[app.runtime.find(&id).unwrap()].terminal.is_none());
        tick_until(app, ctx, "the second sandbox to attach", |app| {
            app.runtime.sessions.iter().find(|s| s.id == id).is_some_and(|s| s.terminal.is_some())
        });
        let create = fake::calls_to(bin.path(), &["create"]).pop().unwrap();
        assert!(create.windows(2).any(|args| args == ["--template", "workspace:local"]));
        finish_agents(app, ctx);
    });
}

#[test]
fn the_cleanup_dialog_removes_a_worktrees_sandbox_and_chosen_orphans() {
    with_app("app::sandbox_tests::the_cleanup_dialog_removes_a_worktrees_sandbox_and_chosen_orphans", |app, ctx| {
        let (_dir, repo) = repository();
        let bin = crate::test_support::tempdir().unwrap();
        let sbx = fake::install(bin.path()).display().to_string();
        app.runtime.sbx = sbx.clone();
        let mut worktrees = Vec::new();
        for name in ["keep", "done", "gone"] {
            let path = worktree(&repo, name);
            let job = crate::sandbox::Prepare {
                home: paths::imacs_home(),
                sbx: sbx.clone(),
                name: format!("imacs-{name}"),
                agent: "claude",
                cwd: path.clone(),
                config: Default::default(),
                warm: Vec::new(),
                exe: "/imacs".into(),
                socket: "/imacs.sock".into(),
                briefs: None,
                pins: paths::git_pins_dir(),
            };
            crate::sandbox::prepare(&job).unwrap();
            worktrees.push(path);
        }
        git::remove_worktree(&repo, &worktrees[2], None, false).unwrap();

        // The dialog's rows name worktrees as git lists them (/private/var for /var on macOS).
        let done = git::list_worktrees(&repo).unwrap().into_iter().find(|w| w.branch.as_deref() == Some("done"));
        app.runtime.open_cleanup(repo.clone(), Some(done.unwrap().path));
        tick_until(app, ctx, "the sandboxes to load", |app| {
            app.runtime.cleanup.as_ref().is_some_and(|c| c.rows.is_some() && !c.sandboxes.orphans.is_empty())
        });
        let cleanup = app.runtime.cleanup.as_ref().unwrap();
        let names = |list: &[(PathBuf, String)]| list.iter().map(|(_, n)| n.clone()).collect::<Vec<_>>();
        assert_eq!(names(&cleanup.sandboxes.attached), ["imacs-done", "imacs-keep"]);
        assert_eq!(cleanup.sandboxes.orphans.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(), ["imacs-gone"]);

        let mut time = 0.0;
        let mut frame = |app: &mut Desktop<'_>, events| {
            time += 0.05;
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1700.0, 1000.0))),
                    time: Some(time),
                    events,
                    ..Default::default()
                },
                |_| app.cleanup_dialog(ctx),
            );
            app.runtime.drain_actions();
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
        frame(app, vec![]);
        app.client.cleanup_sandboxes.insert("imacs-gone".into());
        frame(app, vec![]);
        let text = frame(app, vec![]);
        assert!(text.iter().any(|(s, _)| s == "Sandboxes whose worktree is gone"), "{text:?}");
        let label = "Remove 1 worktree and 1 sandbox";
        let pos = text.iter().find(|(s, _)| s == label).unwrap_or_else(|| panic!("missing {label}: {text:?}")).1;
        for pressed in [true, false] {
            frame(
                app,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }
        tick_until(app, ctx, "the removal", |_| fake::calls_to(bin.path(), &["rm"]).len() == 2);
        assert_eq!(
            fake::calls_to(bin.path(), &["rm"]),
            [["rm", "--force", "imacs-done"], ["rm", "--force", "imacs-gone"]]
        );
        assert!(!worktrees[1].exists());
        assert!(worktrees[0].exists());
        let left: Vec<String> = crate::sandbox::list(&sbx).unwrap().into_iter().map(|s| s.name).collect();
        assert_eq!(left, ["imacs-keep"]);
        // Removed sandboxes take their git records along; the kept worktree stays pinned.
        let records = || {
            let mut names: Vec<_> = std::fs::read_dir(paths::git_pins_dir())
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        tick_until(app, ctx, "the removed sandboxes' git records to go", |_| records() == ["imacs-keep.json"]);
        assert!(git::pinned(&worktrees[0]).is_some() && git::pinned(&worktrees[1]).is_none());
        tick_until(app, ctx, "cleanup to finish", |app| app.runtime.cleanup.as_ref().is_some_and(|c| c.busy.is_none()));
        app.runtime.finish_lifecycle_jobs();
        let fake_home = bin.path().to_path_buf();
        drop(bin);
        assert!(!fake_home.exists(), "the cleanup-dialog test left its fake sbx home behind");
    });
}

#[test]
fn a_missing_or_signed_out_sbx_refuses_the_agent_and_blocks_its_task() {
    with_app("app::sandbox_tests::a_missing_or_signed_out_sbx_refuses_the_agent_and_blocks_its_task", |app, ctx| {
        let (_dir, repo) = repository();
        let worktree = worktree(&repo, "task");
        let bin = crate::test_support::tempdir().unwrap();
        let sbx = fake::install(bin.path()).display().to_string();
        std::fs::write(bin.path().join("signed-out"), "").unwrap();
        app.runtime.config.isolation.mode = IsolationMode::Sandbox;
        for (program, reason) in [(app.runtime.sbx.clone(), "not installed"), (sbx, "not signed in")] {
            app.runtime.sbx = program;
            let id =
                app.runtime.start(ctx, "codex", worktree.clone(), None, vec![], Some("Do the task".into())).unwrap();
            // Dispatch links the task to the session as soon as the tile exists.
            app.runtime.plans = vec![Plan {
                id: "plan".into(),
                tasks: vec![Task {
                    id: "task".into(),
                    status: TaskStatus::Running,
                    session: Some(id.clone()),
                    ..Default::default()
                }],
                ..Default::default()
            }];
            tick_until(app, ctx, "the refusal", |app| {
                app.runtime.sessions.iter().any(|s| s.id == id && s.status == AgentStatus::Exited)
            });
            let session = &app.runtime.sessions[app.runtime.find(&id).unwrap()];
            let message = session.last_message.clone().unwrap();
            assert!(message.contains(reason) && message.contains("mode = \"bare\""), "{message}");
            assert!(session.terminal.is_none() && session.sandbox.is_none(), "nothing ran, sandboxed or bare");
            let task = &app.runtime.plans[0].tasks[0];
            assert_eq!(task.status, TaskStatus::Blocked);
            assert!(task.notes.last().unwrap().contains(reason), "{:?}", task.notes);
            // The session and task above retain the reason. A five-second toast can
            // expire during a loaded runtime tick, so only inspect it while present.
            if let Some((toast, _)) = &app.runtime.toast {
                assert!(toast.contains(reason));
            }
        }
        assert!(fake::calls_to(bin.path(), &["create"]).is_empty());
        assert!(fake::calls_to(bin.path(), &["run"]).is_empty());
        finish_agents(app, ctx);
    });
}

#[test]
fn the_chief_always_runs_on_this_machine() {
    with_app("app::sandbox_tests::the_chief_always_runs_on_this_machine", |app, ctx| {
        let (_dir, repo) = repository();
        let bin = crate::test_support::tempdir().unwrap();
        app.runtime.sbx = fake::install(bin.path()).display().to_string();
        app.runtime.config.isolation.mode = IsolationMode::Sandbox;
        std::fs::create_dir(repo.join(".imacs")).unwrap();
        std::fs::write(repo.join(".imacs/config.toml"), "[isolation]\nmode = \"sandbox\"\n").unwrap();
        app.runtime.view = Some(repo.clone());
        let out = bin.path().join("chief-ran");
        app.runtime.config.chief.command = vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("pwd > {0}.tmp; mv {0}.tmp {0}; exec sleep 600", out.display()),
        ];
        app.runtime.config.chief.args = Vec::new();

        let id = app.runtime.start_chief(ctx, None).unwrap();
        let chief = &app.runtime.sessions[app.runtime.find(&id).unwrap()];
        assert!(chief.chief && chief.terminal.is_some() && chief.sandbox.is_none(), "the chief starts bare at once");
        tick_until(app, ctx, "the chief", |_| out.exists());
        assert_eq!(Path::new(std::fs::read_to_string(&out).unwrap().trim()), repo.canonicalize().unwrap());

        // Resumed after a restart, it stays on this machine too.
        let record = SessionRecord {
            chief: true,
            pane: Some(id),
            claude_session_id: "chief-session".into(),
            agent: "chief".into(),
            cwd: repo.clone(),
            title: "chief".into(),
            last_prompt: None,
            updated: 0,
            sandbox: None,
        };
        app.runtime.sessions.clear();
        std::fs::remove_file(&out).unwrap();
        app.runtime.resume_record(ctx, record);
        assert!(app.runtime.sessions[0].chief && app.runtime.sessions[0].terminal.is_some());
        tick_until(app, ctx, "the resumed chief", |_| out.exists());
        assert!(fake::calls(bin.path()).is_empty(), "the chief never calls sbx");
        app.runtime.sessions.clear();
    });
}

#[test]
fn a_tasks_isolation_wins_over_the_agent_and_mode_but_never_over_a_repository() {
    with_app(
        "app::sandbox_tests::a_tasks_isolation_wins_over_the_agent_and_mode_but_never_over_a_repository",
        |app, ctx| {
            let (dir, repo) = repository();
            let bin = crate::test_support::tempdir().unwrap();
            app.runtime.sbx = fake::install(bin.path()).display().to_string();
            app.runtime.config.isolation.mode = IsolationMode::Sandbox;
            app.runtime.config.base_ref = "main".into();
            app.runtime.config.worktree_path = format!("{}/{{slug}}", dir.path().display());
            let out = bin.path().join("probe-ran");
            app.runtime.config.agents.push(probe("probe", &out, None));

            // The chief marks a task bare, here one sbx has no agent for; the other follows the mode.
            let tasks = json!([
                {"id":"native", "title":"Native build", "brief":"Build the app", "agent":"probe", "isolation":"bare"},
                {"id":"code", "title":"Code", "brief":"Change the code", "agent":"shell"},
            ]);
            let set = request(app, json!({"cmd":"plan_set", "goal":"Mixed", "tasks":tasks}));
            assert!(set.ok, "{set:?}");
            let index = app.runtime.plans.iter().position(|plan| plan.goal == "Mixed").unwrap();
            assert_eq!(app.runtime.plans[index].tasks[0].isolation, Some(IsolationMode::Bare));
            assert_eq!(app.runtime.plans[index].tasks[1].isolation, None);
            app.runtime.plans[index].project = Some(repo.clone());
            app.runtime.plans[index].approved = true;
            let plan = app.runtime.plans[index].id.clone();
            let shown = request(app, json!({"cmd":"plan_get", "plan":plan}));
            assert_eq!(shown.result["tasks"][0]["isolation"], "bare", "{:?}", shown.result);

            app.runtime.dispatch(index, None).unwrap();
            tick_until(app, ctx, "both agents", |_| out.exists() && bin.path().join("run-ssh").exists());
            let session = |app: &Desktop<'_>, task: usize| {
                let id = app.runtime.plans[index].tasks[task].session.clone().unwrap();
                app.runtime.sessions.iter().find(|s| s.id == id).unwrap().sandbox.clone()
            };
            assert_eq!(session(app, 0), None, "the bare task runs on this machine");
            assert!(session(app, 1).is_some_and(|name| name.starts_with("imacs-")));
            assert_eq!(fake::calls_to(bin.path(), &["create"]).len(), 1);

            // After a restart the task keeps its isolation.
            let native = app.runtime.plans[index].tasks[0].session.clone().unwrap();
            let record = SessionRecord {
                chief: false,
                pane: Some(native.clone()),
                claude_session_id: "native-session".into(),
                agent: "probe".into(),
                cwd: app.runtime.plans[index].tasks[0].worktree.clone().unwrap(),
                title: "native".into(),
                last_prompt: None,
                updated: 0,
                sandbox: None,
            };
            app.runtime.close(&native);
            std::fs::remove_file(&out).unwrap();
            app.runtime.resume_record(ctx, record);
            tick_until(app, ctx, "the resumed bare task", |_| out.exists());
            assert_eq!(session(app, 0), None);

            // A repository that requires sandboxes beats the task: the probe cannot run in one.
            std::fs::create_dir(repo.join(".imacs")).unwrap();
            std::fs::write(repo.join(".imacs/config.toml"), "[isolation]\nmode = \"sandbox\"\n").unwrap();
            app.runtime.reload_repo_workflows();
            let resumed = app.runtime.plans[index].tasks[0].session.clone().unwrap();
            app.runtime.close(&resumed);
            std::fs::remove_file(&out).unwrap();
            app.runtime.task_action(index, 0, "resume");
            let toast = app.runtime.toast.as_ref().unwrap().0.clone();
            assert!(toast.contains("requires sandboxed agents"), "{toast}");
            assert!(!out.exists());
            finish_agents(app, ctx);
        },
    );
}

#[test]
fn sandboxed_fake_agent_moving_main_is_restored_and_alerts_the_board() {
    with_app("app::sandbox_tests::sandboxed_fake_agent_moving_main_is_restored_and_alerts_the_board", |app, ctx| {
        let (_dir, repo) = repository();
        let original = git::git(&repo, &["rev-parse", "main"]).unwrap();
        let worktree = worktree(&repo, "task");
        let bin = crate::test_support::tempdir().unwrap();
        app.runtime.sbx = fake::install(bin.path()).display().to_string();
        app.runtime.config.isolation.mode = IsolationMode::Sandbox;
        std::fs::write(bin.path().join("agent"),
            "git -c user.name=Test -c user.email=test@example.org commit --allow-empty -qm task\ngit update-ref refs/heads/main HEAD\n").unwrap();
        let id = app.runtime.start(ctx, "codex", worktree.clone(), None, vec![], Some("Do the task".into())).unwrap();
        app.runtime.plans = vec![Plan {
            id: "guard-plan".into(),
            tasks: vec![Task {
                id: "task".into(),
                session: Some(id.clone()),
                worktree: Some(worktree.clone()),
                branch: Some("task".into()),
                status: TaskStatus::Running,
                ..Default::default()
            }],
            ..Default::default()
        }];
        tick_until(app, ctx, "ref recovery on agent exit", |app| app.runtime.plans[0].tasks[0].ref_alert.is_some());
        assert_eq!(git::git(&repo, &["rev-parse", "main"]).unwrap(), original);
        assert_ne!(git::git(&worktree, &["rev-parse", "HEAD"]).unwrap(), original);
        let alert = app.runtime.plans[0].tasks[0].ref_alert.as_deref().unwrap();
        assert!(alert.contains("SANDBOX REF GUARD") && alert.contains("Restored refs/heads/main"), "{alert}");
        assert!(app.runtime.plans[0].tasks[0].notes.iter().any(|note| note.contains("Restored refs/heads/main")));
        finish_agents(app, ctx);
    });
}

#[test]
fn git_files_a_sandbox_redirects_never_run_its_hook_on_this_machine() {
    with_app("app::sandbox_tests::git_files_a_sandbox_redirects_never_run_its_hook_on_this_machine", |app, ctx| {
        for redirect in ["git-file", "worktree-commondir", "main-commondir"] {
            let (dir, repo) = repository();
            git::git(&repo, &["branch", "other"]).unwrap();
            let original = git::git(&repo, &["rev-parse", "other"]).unwrap();
            let folder = if redirect == "main-commondir" { repo.clone() } else { worktree(&repo, redirect) };
            let marker = dir.path().join("hook-ran");
            let bin = crate::test_support::tempdir().unwrap();
            app.runtime.sbx = fake::install(bin.path()).display().to_string();
            app.runtime.config.isolation.mode = IsolationMode::Sandbox;
            // The agent, as its VM could: commits, moves a branch that is not its own, then plants a git
            // folder whose config runs a hook, and points its worktree's git files at it.
            let tamper = match redirect {
                "git-file" => r#"printf 'gitdir: %s\n' "$evil" > .git"#,
                _ => r#"printf '%s\n' "$evil" > "$gd/commondir""#,
            };
            let script = format!(
                r#"set -e
git commit --allow-empty -qm task
git update-ref refs/heads/other HEAD
gd=$(git rev-parse --path-format=absolute --git-dir)
common=$(git rev-parse --path-format=absolute --git-common-dir)
evil="$gd/evil"
mkdir -p "$evil/hooks"
ln -s "$common/objects" "$evil/objects"
ln -s "$common/refs" "$evil/refs"
cp "$gd/HEAD" "$evil/HEAD"
printf '#!/bin/sh\n: > %s\n' '{marker}' > "$evil/hooks/reference-transaction"
chmod +x "$evil/hooks/reference-transaction"
printf '[core]\n\tfsmonitor = %s\n\thooksPath = %s\n' "$evil/hooks/reference-transaction" "$evil/hooks" > "$evil/config"
echo new > a.txt
{tamper}
"#,
                marker = marker.display()
            );
            std::fs::write(bin.path().join("agent"), script).unwrap();
            let id = app.runtime.start(ctx, "codex", folder.clone(), None, vec![], Some("Do the task".into())).unwrap();
            let name = format!("imacs-{id}");
            // Ref recovery on exit restores the moved branch with git's update-ref.
            tick_until(app, ctx, "ref recovery on agent exit", |app| {
                app.runtime.sessions.iter().any(|s| {
                    s.id == id && s.last_message.as_deref().is_some_and(|m| m.contains("Restored refs/heads/other"))
                })
            });
            assert_eq!(git::git(&repo, &["rev-parse", "other"]).unwrap(), original, "{redirect}");
            let record: git::GitPins =
                serde_json::from_slice(&std::fs::read(paths::git_pins_dir().join(format!("{name}.json"))).unwrap())
                    .unwrap();
            assert_eq!(record.work_tree, folder.canonicalize().unwrap());
            assert_eq!(record.common_dir, Some(repo.join(".git").canonicalize().unwrap()));

            // The diff IMACS polls, and what worktree cleanup asks of git.
            let diff = git::diff_against_base(&folder, "main", None).unwrap();
            assert_eq!(diff.files.iter().map(|f| f.path()).collect::<Vec<_>>(), ["a.txt"], "{redirect}");
            let health = git::worktree_health(&repo, "main").unwrap();
            if redirect != "main-commondir" {
                assert_eq!(health[0].dirty_files, 1, "{redirect}: {health:?}");
                assert!(git::remove_worktree(&repo, &folder, None, false).is_err(), "{redirect}: uncommitted a.txt");
            }
            app.runtime.close(&id);
            tick_until(app, ctx, "the final ref check", |_| !fake::calls_to(bin.path(), &["stop"]).is_empty());
            assert!(!marker.exists(), "{redirect}: host git ran the sandbox's hook");

            // Without its record, git follows the planted files to that hook.
            let raw = std::process::Command::new("git").arg("-C").arg(&folder).args(["status"]).output().unwrap();
            assert!(raw.status.success() && marker.exists(), "{redirect}: the planted config must be live");
            app.runtime.sessions.clear();
        }
    });
}

#[test]
fn sandboxed_codex_trusts_its_folder_and_a_first_run_question_needs_you() {
    with_app("app::sandbox_tests::sandboxed_codex_trusts_its_folder_and_a_first_run_question_needs_you", |app, ctx| {
        let (_dir, repo) = repository();
        let worktree = worktree(&repo, "task.v2");
        let bin = crate::test_support::tempdir().unwrap();
        app.runtime.sbx = fake::install(bin.path()).display().to_string();
        app.runtime.config.isolation.mode = IsolationMode::Sandbox;
        // A Codex that still asks, as one too old for the trust setting would.
        std::fs::write(
            bin.path().join("agent"),
            "printf '  Trust this folder? Codex can read, edit, and run files here.\\n\\n› 1. Trust and continue\\n  2. Open restricted\\n'\nexec sleep 30\n",
        )
        .unwrap();
        let id = app.runtime.start(ctx, "codex", worktree.clone(), None, vec![], Some("Do the task".into())).unwrap();
        tick_until(app, ctx, "the agent to attach", |_| !fake::calls_to(bin.path(), &["run"]).is_empty());

        // The mounted worktree is trusted up front; its dotted name stays one key of the table.
        let run = fake::calls_to(bin.path(), &["run"]).pop().unwrap();
        let folder = worktree.canonicalize().unwrap().display().to_string();
        assert_eq!(run[4..7], ["--", "-c", &format!("projects={{ {folder:?} = {{ trust_level = \"trusted\" }} }}")]);
        assert_eq!(run.last().map(String::as_str), Some("Do the task"));
        let projects: toml::Table = toml::from_str(&run[6]).unwrap();
        assert_eq!(projects["projects"][&folder]["trust_level"].as_str(), Some("trusted"));

        // A question before the first turn needs you, instead of the tile saying idle.
        tick_until(app, ctx, "the question to need you", |app| {
            app.runtime.busy_checked = Instant::now() - Duration::from_secs(1);
            app.runtime.sessions[app.runtime.find(&id).unwrap()].status == AgentStatus::NeedsInput
        });
        assert!(app.runtime.events.iter().any(|e| e.session == id && e.status == AgentStatus::NeedsInput));
        app.runtime.close(&id);
    });
}

#[test]
fn sandbox_briefs_are_mounted_read_only_and_reports_use_the_sandbox_identity() {
    with_app(
        "app::sandbox_tests::sandbox_briefs_are_mounted_read_only_and_reports_use_the_sandbox_identity",
        |app, ctx| {
            let (_dir, repo) = repository();
            let worktree = worktree(&repo, "task");
            let bin = crate::test_support::tempdir().unwrap();
            app.runtime.sbx = fake::install(bin.path()).display().to_string();
            app.runtime.config.isolation.mode = IsolationMode::Sandbox;
            std::fs::write(bin.path().join("run-waits"), "").unwrap();
            let config = app
                .runtime
                .config
                .isolation
                .sandbox
                .with_defaults(&app.runtime.config.resources, app.runtime.governor.cores);
            let task = Task { id: "task".into(), brief: "Read this long brief. ".repeat(80), ..Default::default() };
            let text = task.compose_brief_for("Sandbox task", Some(&config));
            assert!(!text.contains("imacs queue"));
            assert!(text.contains("CPUs and 8g memory"));
            let id = app.runtime.start(ctx, "codex", worktree, None, vec![], Some(text.clone())).unwrap();
            tick_until(app, ctx, "the sandboxed agent", |_| bin.path().join("run-ssh").exists());
            let name = format!("imacs-{id}");
            let brief = paths::briefs_dir().join(&id).join("brief.md");
            assert_eq!(std::fs::read_to_string(&brief).unwrap(), text);
            let create = fake::calls_to(bin.path(), &["create"]).pop().unwrap();
            assert!(create.contains(&format!("{}:ro", brief.parent().unwrap().display())), "{create:?}");
            let run = fake::calls_to(bin.path(), &["run"]).pop().unwrap();
            assert!(run.last().unwrap().contains(&brief.display().to_string()));
            let report = request(app, json!({"cmd":"report", "pane":name, "status":"done", "summary":"VM finished"}));
            assert!(report.ok, "{report:?}");
            let session = &app.runtime.sessions[app.runtime.find(&id).unwrap()];
            assert_eq!(session.status, AgentStatus::Done);
            assert!(app.runtime.events.iter().any(|event| event.session == id && event.status == AgentStatus::Done));
            assert!(session.approvals.is_empty());
            app.runtime.close(&id);
            app.runtime.finish_lifecycle_jobs();
        },
    );
}

#[test]
fn sandbox_screen_fallback_debounces_quiet_and_respects_reports_until_the_next_turn() {
    with_app(
        "app::sandbox_tests::sandbox_screen_fallback_debounces_quiet_and_respects_reports_until_the_next_turn",
        |app, ctx| {
            let (_dir, repo) = repository();
            let worktree = worktree(&repo, "task");
            for (agent, kind) in [("claude", AgentKind::Claude), ("codex", AgentKind::Generic)] {
                let mut session = super::tests::session(agent, kind, AgentStatus::Starting, &worktree);
                session.agent = agent.into();
                session.sandbox = Some(format!("imacs-{agent}"));
                // Stale host hooks must not disable screen detection inside the VM.
                session.hooks_active = true;
                app.runtime.sessions = vec![session];
                let screen = |app: &mut Desktop<'_>, text: &str| {
                    let terminal = Terminal::spawn(
                        app.runtime.wake.clone(),
                        app.runtime.palette.terminal_colors(),
                        "/bin/sh".into(),
                        vec!["-c".into(), format!("printf '%s' '{text}'; exec sleep 600")],
                        &worktree,
                        HashMap::new(),
                        100,
                    )
                    .unwrap();
                    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(60));
                    while !terminal.screen_text(60).contains(text) {
                        assert!(Instant::now() < deadline, "screen did not appear");
                        app.runtime.wake.wait_until(deadline.min(Instant::now() + Duration::from_millis(100)));
                    }
                    app.runtime.sessions[0].terminal = Some(terminal);
                };
                let track = |app: &mut Desktop<'_>| {
                    app.runtime.busy_checked = Instant::now() - Duration::from_secs(1);
                    app.runtime.track_busy();
                };
                screen(app, "Working (Esc to interrupt)");
                track(app);
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::Working, "{agent}");
                screen(app, "Ready");
                track(app);
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::Working, "transient redraw");
                app.runtime.sessions[0].busy_absent_since = Some(Instant::now() - Duration::from_secs(1));
                track(app);
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::Done);
                screen(app, "Working (esc to interrupt)");
                track(app);
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::Working);
                for (status, expected) in [("done", AgentStatus::Done), ("question", AgentStatus::NeedsInput)] {
                    let pane = format!("imacs-{agent}");
                    assert!(request(app, json!({"cmd":"report","pane":pane,"status":status,"summary":"VM report"})).ok);
                    track(app);
                    assert_eq!(app.runtime.sessions[0].status, expected, "stale busy footer cannot override a report");
                }
                screen(app, "Ready");
                app.runtime.sessions[0].busy_absent_since = Some(Instant::now() - Duration::from_secs(1));
                track(app);
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::NeedsInput);
                screen(app, "Working (esc to interrupt)");
                track(app);
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::Working, "new turn resumes screen detection");
                app.runtime.runtime_tick(ctx);
                assert!(app.runtime.sessions[0].approvals.is_empty());
                app.runtime.sessions.clear();
            }
        },
    );
}

#[test]
fn worktrees_are_listed_off_the_ui_thread_and_a_planted_one_never_shows() {
    with_app("app::sandbox_tests::worktrees_are_listed_off_the_ui_thread_and_a_planted_one_never_shows", |app, ctx| {
        use std::os::unix::fs::OpenOptionsExt;
        let (_dir, repo) = repository();
        let task = worktree(&repo, "task");
        let peer = worktree(&repo, "peer");
        let pins = git::resolve_pins(&task).unwrap();
        git::pin(pins.clone());
        // The task's sandbox names a folder in its own git folder as its worktree.
        let admin = pins.git_dir.clone().unwrap();
        std::fs::create_dir(admin.join("evil")).unwrap();
        std::fs::write(admin.join("gitdir"), format!("{}\n", admin.join("evil/.git").display())).unwrap();
        // A FIFO no check sees, as one swapped in after the check would be: git waits on it.
        let peer_admin = git::resolve_pins(&peer).unwrap().git_dir.unwrap();
        let fifo = peer_admin.join("locked");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        app.runtime.repos = vec![repo.clone()];
        app.runtime.worktrees_at = None;
        for _ in 0..3 {
            let started = Instant::now();
            app.runtime.runtime_tick(ctx);
            assert!(started.elapsed() < Duration::from_secs(2), "the UI thread waited for git");
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(app.runtime.worktree_listing.is_some() && app.runtime.worktrees.is_empty());

        // Its writer lets git go on; it opens without waiting for git to read.
        let released = Arc::new(AtomicBool::new(false));
        let writer = {
            let (fifo, released) = (fifo.clone(), released.clone());
            std::thread::spawn(move || {
                while !released.load(Ordering::Relaxed) {
                    drop(std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(&fifo));
                    std::thread::sleep(Duration::from_millis(10));
                }
            })
        };
        tick_until(app, ctx, "the worktree list", |app| !app.runtime.worktrees.is_empty());
        released.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        let listed: Vec<_> = app.runtime.worktrees.iter().map(|w| w.branch.clone().unwrap_or_default()).collect();
        assert_eq!(listed, ["main", "peer"], "the planted worktree must never show");
        git::unpin(&pins.work_tree);
    });
}
