use super::tests::{persist_config, repository, request, session, with_app};
use super::*;

#[test]
fn chief_task_review_patch_invalidates_the_reviewed_digest() {
    let mut plan = Plan::default();
    plan.replace(
        "Review scope".into(),
        vec![Task { id: "a".into(), ..Default::default() }],
        &atomics_core::config::Config::default(),
    );
    plan.approved = true;
    let before = plan.digest();
    plan.patch_task("review", json!({"args":["--model","chosen-reviewer"]})).unwrap();
    assert_ne!(plan.digest(), before, "a stale user approval must not approve an edited reviewer");
    assert!(plan.patch_task("review", json!({"depends_on":[]})).is_err());
}

#[test]
fn patch_and_add_only_reapprove_the_changed_task() {
    with_app("app::chief_tools_tests::patch_and_add_only_reapprove_the_changed_task", |app, _| {
        let (_dir, repo) = repository();
        app.runtime.config.base_ref = "main".into();
        app.runtime.config.worktree_path = "{repo_parent}/{repo}-{slug}".into();
        let set = request(
            app,
            json!({"cmd":"plan_set","goal":"Focused edits","tasks":[{"id":"a","brief":"original","repo":repo},{"id":"b","brief":"untouched","repo":repo}]}),
        );
        let id = set.result["plan"].clone();
        request(app, json!({"cmd":"plan_approve","plan":id}));
        let patched = request(app, json!({"cmd":"plan_patch","plan":id,"id":"a","patch":{"brief":"corrected"}}));
        assert!(patched.ok, "{patched:?}");
        let read = request(app, json!({"cmd":"plan_get","plan":id}));
        assert_eq!(read.result["tasks"][0]["approved"], false);
        assert_eq!(read.result["tasks"][1]["approved"], true);
        assert_eq!(read.result["tasks"].as_array().unwrap().last().unwrap()["approved"], true);
        assert!(read.result["ready"].as_array().unwrap().contains(&json!("b")));
        assert!(!read.result["ready"].as_array().unwrap().contains(&json!("a")));
        let added = request(app, json!({"cmd":"plan_add","plan":id,"task":{"id":"c","brief":"new task"}}));
        assert!(added.ok, "{added:?}");
        let read = request(app, json!({"cmd":"plan_get","plan":id,"task":"b"}));
        assert_eq!(read.result["task"]["approved"], true);
        let read = request(app, json!({"cmd":"plan_get","plan":id,"task":"c"}));
        assert_eq!(read.result["task"]["approved"], false);
        let saved =
            Plan::load(&paths::atomics_home().join("plans").join(format!("{}.json", id.as_str().unwrap()))).unwrap();
        assert!(saved.task_approved(&saved.tasks[1]));
        assert!(!saved.task_approved(saved.tasks.iter().find(|task| task.id == "c").unwrap()));
        let invalid = request(app, json!({"cmd":"plan_patch","plan":id,"id":"b","patch":{"depends_on":["review"]}}));
        assert!(!invalid.ok);
        assert_eq!(app.runtime.plans.iter().find(|p| p.id == id).unwrap().tasks[1].brief, "untouched");
        let forged = request(app, json!({"cmd":"plan_patch","plan":id,"id":"b","patch":{"approved":true}}));
        assert!(!forged.ok, "host fields cannot be patched");
        let index = app.runtime.plans.iter().position(|plan| plan.id == id).unwrap();
        assert_eq!(app.runtime.dispatch(index, Some("b")).unwrap(), ["b"]);
        let (_, prepared, _, _) =
            app.runtime.worktree_rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(10))).unwrap();
        assert!(prepared.is_ok(), "an unchanged approved task can still dispatch");
    });
}

#[test]
fn agent_and_model_patch_keeps_live_session_and_needs_task_approval() {
    with_app("app::chief_tools_tests::agent_and_model_patch_keeps_live_session_and_needs_task_approval", |app, _| {
        let root = app.runtime.launch_dir.clone();
        app.runtime.sessions = vec![session("worker", AgentKind::Claude, AgentStatus::Idle, &root)];
        app.runtime.plans = vec![Plan {
            id: "board".into(),
            approved: true,
            tasks: vec![
                Task {
                    id: "a".into(),
                    agent: "claude".into(),
                    session: Some("worker".into()),
                    status: TaskStatus::Running,
                    ..Default::default()
                },
                Task { id: "b".into(), ..Default::default() },
            ],
            ..Default::default()
        }];
        let patched = request(
            app,
            json!({"cmd":"plan_patch","plan":"board","id":"a","patch":{"agent":"codex","args":["--model","test-model"]}}),
        );
        assert!(patched.ok, "{patched:?}");
        let read = request(app, json!({"cmd":"plan_get","plan":"board","task":"a"}));
        assert_eq!(read.result["task"]["agent"], "codex");
        assert_eq!(read.result["task"]["args"], json!(["--model", "test-model"]));
        assert_eq!(read.result["task"]["approved"], false);
        assert_eq!(read.result["task"]["session"], "worker");
        assert_eq!(app.runtime.sessions[0].agent, "test", "a patch cannot restart the agent");
    });
}

#[test]
fn add_and_start_waits_for_user_approval_then_launches_a_linked_task() {
    with_app("app::chief_tools_tests::add_and_start_waits_for_user_approval_then_launches_a_linked_task", |app, _| {
        let (_dir, repo) = repository();
        app.runtime.config.base_ref = "main".into();
        app.runtime.config.isolation.mode = atomics_core::isolation::IsolationMode::Bare;
        app.runtime.config.worktree_path = "{repo_parent}/{repo}-{slug}".into();
        app.runtime.config.agents.push(atomics_core::config::AgentConfig {
            name: "probe".into(),
            command: vec!["/bin/cat".into()],
            kind: AgentKind::Shell,
            busy_text: None,
            isolation: None,
        });
        persist_config(app.runtime);
        app.runtime.plans =
            vec![Plan { id: "board".into(), project: Some(repo), approved: true, ..Default::default() }];
        let added = request(
            app,
            json!({"cmd":"plan_add","plan":"board","task":{"id":"quick","title":"Ad hoc","brief":"Check the parser","agent":"probe"},"start":true}),
        );
        assert!(added.ok, "{added:?}");
        assert!(
            app.runtime.creating.is_empty() && app.runtime.sessions.is_empty(),
            "an agent's start flag grants no approval"
        );
        let read = request(app, json!({"cmd":"plan_get","plan":"board","task":"quick"}));
        assert_eq!(read.result["task"]["start_on_approval"], true);
        request(app, json!({"cmd":"plan_approve","plan":"board"}));
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(15));
        while app.runtime.plans[0].tasks[0].session.as_ref().is_none_or(|id| {
            app.runtime
                .sessions
                .iter()
                .find(|worker| &worker.id == id)
                .is_none_or(|worker| worker.worktree_ready.is_some())
        }) {
            assert!(Instant::now() < deadline, "approved ad-hoc task did not start");
            app.runtime.runtime_tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        let task = &app.runtime.plans[0].tasks[0];
        let pane = task.session.clone().unwrap();
        let worker = app.runtime.sessions.iter().find(|s| s.id == pane).unwrap();
        assert_eq!(task.worktree.as_ref().unwrap().canonicalize().unwrap(), worker.root().canonicalize().unwrap());
        assert_eq!(task.branch.as_deref(), worker.branch());
        assert_eq!(app.runtime.report(Some(&pane), "done", "Ad-hoc result").as_deref(), Some("quick"));
        assert!(app.runtime.events.last().unwrap().task.as_deref() == Some("quick"));
    });
}

#[test]
fn approval_changes_agent_resume_uses_approved_model() {
    with_app("app::chief_tools_tests::approval_changes_agent_resume_uses_approved_model", |app, _| {
        let (dir, repo) = repository();
        let output = dir.path().join("args");
        app.runtime.config.isolation.mode = atomics_core::isolation::IsolationMode::Bare;
        app.runtime.config.agents.push(atomics_core::config::AgentConfig {
            name: "new-agent".into(),
            kind: AgentKind::Generic,
            busy_text: None,
            isolation: None,
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                format!("printf '%s\\n' \"$@\" > '{}'; exec /bin/cat", output.display()),
                "probe".into(),
            ],
        });
        persist_config(app.runtime);
        app.runtime.plans = vec![Plan {
            id: "board".into(),
            approved: true,
            tasks: vec![Task {
                id: "a".into(),
                agent: "old-agent".into(),
                worktree: Some(repo),
                resume_id: Some("old-conversation".into()),
                status: TaskStatus::Blocked,
                ..Default::default()
            }],
            ..Default::default()
        }];
        let patched = request(
            app,
            json!({"cmd":"plan_patch","plan":"board","id":"a","patch":{"agent":"new-agent","args":["--model","chosen-model"]}}),
        );
        assert!(patched.ok, "{patched:?}");
        app.runtime.task_action(0, 0, "resume");
        assert!(app.runtime.sessions.is_empty(), "unapproved changes cannot resume");
        request(app, json!({"cmd":"plan_approve","plan":"board"}));
        app.runtime.task_action(0, 0, "resume");
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(15));
        while !output.exists() {
            assert!(Instant::now() < deadline, "new agent did not start");
            app.runtime.runtime_tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        let args = std::fs::read_to_string(output).unwrap();
        assert!(args.contains("chosen-model"), "{args}");
        assert!(
            !args.contains("old-conversation"),
            "a different agent must not resume its predecessor's conversation: {args}"
        );
        assert_eq!(app.runtime.sessions.last().unwrap().agent, "new-agent");
    });
}

#[test]
fn narrow_then_broad_wait_keeps_every_report() {
    with_app("app::chief_tools_tests::narrow_then_broad_wait_keeps_every_report", |app, _| {
        app.runtime.report(Some("worker"), "progress", "first report");
        app.runtime.plans = vec![Plan { id: "board".into(), ..Default::default() }];
        app.runtime.plan_event(0, "approved");
        app.runtime.report(Some("worker"), "done", "last report");
        let narrow = request(app, json!({"cmd":"wait","after":0,"statuses":["plan"],"timeout_secs":0}));
        assert_eq!(narrow.result["events"].as_array().unwrap().len(), 1);
        assert_eq!(narrow.result["cursor"], 0, "the report before the plan event is unread");
        let continued = request(
            app,
            json!({"cmd":"wait","after":narrow.result["filter_cursor"],"statuses":["plan"],"timeout_secs":0}),
        );
        assert!(continued.result["events"].as_array().unwrap().is_empty());
        let broad = request(app, json!({"cmd":"wait","after":narrow.result["cursor"],"timeout_secs":0}));
        let reports: Vec<_> = broad.result["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["report"]["summary"].as_str().unwrap())
            .collect();
        assert_eq!(reports, ["first report", "last report"]);
    });
}

#[test]
fn wait_reports_are_one_line_and_full_reports_are_on_demand() {
    with_app("app::chief_tools_tests::wait_reports_are_one_line_and_full_reports_are_on_demand", |app, _| {
        let root = app.runtime.launch_dir.clone();
        app.runtime.sessions = vec![session("worker", AgentKind::Generic, AgentStatus::Idle, &root)];
        app.runtime.plans = vec![Plan {
            id: "board".into(),
            tasks: vec![Task { id: "a".into(), session: Some("worker".into()), ..Default::default() }],
            ..Default::default()
        }];
        let summary = format!("Finished the change.\n\n{}", "long evidence ".repeat(400));
        app.runtime.report(Some("worker"), "done", &summary);
        let waited = request(app, json!({"cmd":"wait","after":0,"timeout_secs":0}));
        let short = waited.result["events"][0]["report"]["summary"].as_str().unwrap();
        assert!(!short.contains('\n') && short.chars().count() <= 240, "{short}");
        let full = request(app, json!({"cmd":"plan_get","plan":"board","task":"a"}));
        assert_eq!(full.result["task"]["report"]["summary"], summary);
        let result = request(app, json!({"cmd":"result","session":"worker"}));
        assert_eq!(result.result["report"]["summary"], summary);
    });
}

#[test]
fn relink_moves_the_tasks_branch_worktree_and_clears_stale_integration() {
    with_app(
        "app::chief_tools_tests::relink_moves_the_tasks_branch_worktree_and_clears_stale_integration",
        |app, _| {
            let (_dir, repo) = repository();
            let root = repo.parent().unwrap().join("new-tree");
            git::add_worktree(&repo, &root, "new-branch", "main").unwrap();
            let mut worker = session("worker", AgentKind::Generic, AgentStatus::Idle, &root);
            worker.worktree = Some(git::Worktree { root: root.clone(), branch: Some("new-branch".into()) });
            app.runtime.sessions = vec![worker];
            app.runtime.plans = vec![Plan {
                id: "board".into(),
                tasks: vec![Task {
                    id: "a".into(),
                    branch: Some("old-branch".into()),
                    worktree: Some(repo),
                    integrated: Some("old-integration".into()),
                    integrated_commit: Some("old-tip".into()),
                    integration_verified: true,
                    ..Default::default()
                }],
                ..Default::default()
            }];
            let linked = request(app, json!({"cmd":"plan_update","plan":"board","id":"a","session":"worker"}));
            assert!(linked.ok, "{linked:?}");
            let task = &app.runtime.plans[0].tasks[0];
            assert_eq!(task.branch.as_deref(), Some("new-branch"));
            assert_eq!(task.worktree.as_ref(), Some(&root));
            assert!(task.integrated.is_none() && task.integrated_commit.is_none() && !task.integration_verified);
            let saved = Plan::load(&paths::atomics_home().join("plans/board.json")).unwrap();
            assert_eq!(saved.tasks[0].worktree.as_ref(), Some(&root));
        },
    );
}

#[test]
fn chief_messages_are_recorded_on_the_task_but_raw_keys_are_not() {
    with_app("app::chief_tools_tests::chief_messages_are_recorded_on_the_task_but_raw_keys_are_not", |app, _| {
        let root = app.runtime.launch_dir.clone();
        app.runtime.sessions = vec![session("worker", AgentKind::Claude, AgentStatus::Idle, &root)];
        app.runtime.plans = vec![Plan {
            id: "board".into(),
            tasks: vec![Task { id: "a".into(), session: Some("worker".into()), ..Default::default() }],
            ..Default::default()
        }];
        let sent = request(app, json!({"cmd":"send","session":"worker","text":"Also cover empty inputs."}));
        assert!(sent.ok, "{sent:?}");
        let full = request(app, json!({"cmd":"plan_get","plan":"board","task":"a"}));
        assert!(full.result["task"]["notes"].to_string().contains("Also cover empty inputs."));
        let notes = app.runtime.plans[0].tasks[0].notes.clone();
        request(app, json!({"cmd":"send","session":"worker","text":"\\e","raw":true}));
        assert_eq!(app.runtime.plans[0].tasks[0].notes, notes);
    });
}
