//! Agents in Docker Sandboxes, against the fake `sbx` in tests/fixtures.
use super::tests::{repository, with_app};
use super::*;
use crate::sandbox::fake;
use imacs_core::isolation::IsolationMode;

/// Ticks IMACS until `done`, failing after ten seconds with `what`.
fn tick_until(app: &mut Desktop<'_>, ctx: &egui::Context, what: &str, mut done: impl FnMut(&mut Desktop<'_>) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done(app) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        app.runtime.runtime_tick(ctx);
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn worktree(repo: &Path, name: &str) -> PathBuf {
    let path = repo.parent().unwrap().join(format!("repo-{name}"));
    git::add_worktree(repo, &path, name, "main").unwrap();
    path
}

fn probe(name: &str, out: &Path, isolation: Option<IsolationMode>) -> imacs_core::config::AgentConfig {
    imacs_core::config::AgentConfig {
        name: name.into(),
        command: vec!["/bin/sh".into(), "-c".into(), format!("pwd > {}; sleep 5", out.display())],
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
        let bin = tempfile::tempdir().unwrap();
        app.runtime.sbx = fake::install(bin.path()).display().to_string();
        app.runtime.config.isolation.mode = IsolationMode::Sandbox;

        let id = app.runtime.start(ctx, "claude", worktree.clone(), None, vec![], Some("Fix the bug".into())).unwrap();
        let name = format!("imacs-{id}");
        let index = app.runtime.find(&id).unwrap();
        assert!(app.runtime.sessions[index].terminal.is_none(), "the tile waits for its sandbox");
        tick_until(app, ctx, "the agent to attach", |_| bin.path().join("run-ssh").exists());

        let create = &fake::calls_to(bin.path(), &["create"])[0];
        assert_eq!(create[..3], ["create", "--name", name.as_str()]);
        let agent = create.iter().position(|arg| arg == "claude").unwrap();
        assert_eq!(Path::new(&create[agent + 1]), worktree.canonicalize().unwrap());
        let run = &fake::calls_to(bin.path(), &["run"])[0];
        assert_eq!(run[..5], ["run", "--name", name.as_str(), "claude", "--"]);
        let session_id = app.runtime.sessions[index].claude_id.clone().unwrap();
        assert_eq!(run[5..], ["--session-id", session_id.as_str(), "Fix the bug"]);
        assert!(!run.iter().any(|arg| arg == "--settings" || arg == "--mcp-config"), "host hooks stay out: {run:?}");
        assert_eq!(std::fs::read_to_string(bin.path().join("run-ssh")).unwrap().trim(), "unset");
        assert_eq!(app.runtime.sessions[index].sandbox.as_deref(), Some(name.as_str()));
        assert_eq!(app.runtime.pane_session(Some(name.clone())), Some(id.clone()), "reports find their session");
        let record = app.runtime.store.sessions.iter().find(|r| r.claude_session_id == session_id).unwrap().clone();
        assert_eq!(record.sandbox.as_deref(), Some(name.as_str()));

        // The agent exits, and its sandbox stops until the next `sbx run`.
        tick_until(app, ctx, "the sandbox to stop", |_| !fake::calls_to(bin.path(), &["stop"]).is_empty());
        assert_eq!(fake::calls_to(bin.path(), &["stop"]), [["stop", name.as_str()]]);

        // A restart resumes the session in the same sandbox, without making another.
        app.runtime.sessions.clear();
        std::fs::remove_file(bin.path().join("run-ssh")).unwrap();
        app.runtime.resume_record(ctx, record);
        tick_until(app, ctx, "the resumed agent to attach", |_| bin.path().join("run-ssh").exists());
        assert_eq!(fake::calls_to(bin.path(), &["create"]).len(), 1);
        let run = fake::calls_to(bin.path(), &["run"]).pop().unwrap();
        assert_eq!(run, ["run", "--name", name.as_str(), "claude", "--", "--resume", session_id.as_str()]);
        assert_eq!(app.runtime.sessions[0].sandbox.as_deref(), Some(name.as_str()));
        app.runtime.sessions.clear();
    });
}

#[test]
fn sandboxed_task_args_drop_host_sandbox_flags_and_bare_ones_keep_them() {
    with_app("app::sandbox_tests::sandboxed_task_args_drop_host_sandbox_flags_and_bare_ones_keep_them", |app, ctx| {
        let (_dir, repo) = repository();
        let worktree = worktree(&repo, "task");
        let bin = tempfile::tempdir().unwrap();
        app.runtime.sbx = fake::install(bin.path()).display().to_string();
        app.runtime.config.isolation.mode = IsolationMode::Sandbox;
        // The arguments IMACS plans give a Codex task, and an agent command carrying a host flag.
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
        app.runtime.start(ctx, "codex", worktree.clone(), None, task_args.clone(), Some("Do the task".into())).unwrap();
        tick_until(app, ctx, "the agent to attach", |_| bin.path().join("run-ssh").exists());
        let run = fake::calls_to(bin.path(), &["run"]).pop().unwrap();
        assert_eq!(
            run[4..],
            ["--", "--search", "-m", "gpt-6.1-sol", "-c", "model_reasoning_effort=high", "Do the task"],
            "{run:?}"
        );

        // A bare agent gets every argument, as before.
        let out = bin.path().join("args");
        let script = bin.path().join("agent");
        std::fs::write(&script, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > {}.tmp\nmv {0}.tmp {0}\n", out.display()))
            .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        app.runtime.config.agents.push(imacs_core::config::AgentConfig {
            name: "bare-agent".into(),
            command: vec![script.display().to_string()],
            kind: AgentKind::Generic,
            busy_text: None,
            isolation: Some(IsolationMode::Bare),
        });
        app.runtime.start(ctx, "bare-agent", worktree.clone(), None, task_args.clone(), None).unwrap();
        tick_until(app, ctx, "the bare agent", |_| out.exists());
        let given: Vec<String> = std::fs::read_to_string(&out).unwrap().lines().map(String::from).collect();
        assert!(given.ends_with(&task_args), "{given:?}");
        app.runtime.sessions.clear();
    });
}

#[test]
fn bare_agents_never_touch_sbx_and_a_repository_can_only_tighten() {
    with_app("app::sandbox_tests::bare_agents_never_touch_sbx_and_a_repository_can_only_tighten", |app, ctx| {
        let (_dir, repo) = repository();
        let worktree = worktree(&repo, "task");
        let bin = tempfile::tempdir().unwrap();
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
        std::fs::create_dir(repo.join(".imacs")).unwrap();
        std::fs::write(repo.join(".imacs/config.toml"), "[isolation]\nmode = \"sandbox\"\n").unwrap();
        let error = app.runtime.start(ctx, "bare-probe", worktree.clone(), None, vec![], None).unwrap_err();
        assert!(error.contains("requires sandboxed agents"), "{error}");
        assert!(!out.exists());
        let id = app.runtime.start(ctx, "shell", worktree.clone(), None, vec![], None).unwrap();
        tick_until(app, ctx, "the sandboxed shell", |_| bin.path().join("run-ssh").exists());
        let run = fake::calls_to(bin.path(), &["run"]).pop().unwrap();
        assert_eq!(run, ["run", "--name", format!("imacs-{id}").as_str(), "shell", "--"]);

        // A repository asking for bare is ignored: sandbox mode stays.
        std::fs::write(repo.join(".imacs/config.toml"), "[isolation]\nmode = \"bare\"\n").unwrap();
        app.runtime.config.isolation.mode = IsolationMode::Sandbox;
        let calls = fake::calls(bin.path()).len();
        let id = app.runtime.start(ctx, "shell", worktree.clone(), None, vec![], None).unwrap();
        assert!(app.runtime.sessions[app.runtime.find(&id).unwrap()].terminal.is_none());
        tick_until(app, ctx, "the second sandbox", |_| fake::calls(bin.path()).len() > calls + 3);
        app.runtime.sessions.clear();
    });
}

#[test]
fn the_cleanup_dialog_removes_a_worktrees_sandbox_and_chosen_orphans() {
    with_app("app::sandbox_tests::the_cleanup_dialog_removes_a_worktrees_sandbox_and_chosen_orphans", |app, ctx| {
        let (_dir, repo) = repository();
        let bin = tempfile::tempdir().unwrap();
        let sbx = fake::install(bin.path()).display().to_string();
        app.runtime.sbx = sbx.clone();
        let mut worktrees = Vec::new();
        for name in ["keep", "done", "gone"] {
            let path = worktree(&repo, name);
            let job = crate::sandbox::Prepare {
                sbx: sbx.clone(),
                name: format!("imacs-{name}"),
                agent: "claude",
                cwd: path.clone(),
                config: Default::default(),
                exe: "/imacs".into(),
                socket: "/imacs.sock".into(),
                briefs: None,
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
    });
}

#[test]
fn a_missing_or_signed_out_sbx_refuses_the_agent_and_blocks_its_task() {
    with_app("app::sandbox_tests::a_missing_or_signed_out_sbx_refuses_the_agent_and_blocks_its_task", |app, ctx| {
        let (_dir, repo) = repository();
        let worktree = worktree(&repo, "task");
        let bin = tempfile::tempdir().unwrap();
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
            assert!(app.runtime.toast.as_ref().unwrap().0.contains(reason));
        }
        assert!(fake::calls_to(bin.path(), &["create"]).is_empty());
        assert!(fake::calls_to(bin.path(), &["run"]).is_empty());
        app.runtime.sessions.clear();
    });
}

#[test]
fn the_chief_always_runs_on_this_machine() {
    with_app("app::sandbox_tests::the_chief_always_runs_on_this_machine", |app, ctx| {
        let (_dir, repo) = repository();
        let bin = tempfile::tempdir().unwrap();
        app.runtime.sbx = fake::install(bin.path()).display().to_string();
        app.runtime.config.isolation.mode = IsolationMode::Sandbox;
        std::fs::create_dir(repo.join(".imacs")).unwrap();
        std::fs::write(repo.join(".imacs/config.toml"), "[isolation]\nmode = \"sandbox\"\n").unwrap();
        app.runtime.view = Some(repo.clone());
        let out = bin.path().join("chief-ran");
        app.runtime.config.chief.command =
            vec!["/bin/sh".into(), "-c".into(), format!("pwd > {0}.tmp; mv {0}.tmp {0}; sleep 5", out.display())];
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
