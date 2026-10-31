use super::*;

pub(super) fn session(id: &str, kind: AgentKind, status: AgentStatus, cwd: &Path) -> Session {
    Session {
        id: id.into(),
        agent: "test".into(),
        kind,
        terminal: None,
        claude_id: Some(id.into()),
        cwd: cwd.into(),
        worktree: None,
        worktree_ready: None,
        title: id.into(),
        status,
        last_prompt: None,
        last_message: None,
        hooks_active: false,
        activity: heartbeat::Activity::default(),
        approvals: Vec::new(),
        allowed: Vec::new(),
        held: Vec::new(),
        approval_hold: false,
        menu_absent_since: None,
        chief: false,
        pending: None,
        sandbox: None,
        relay: None,
        screen_busy: false,
        busy_absent_since: None,
        ref_guard: None,
        scratch: None,
        naming: Naming::Folder,
        mcp_needed: Vec::new(),
    }
}

pub(super) fn with_app(name: &str, test: impl FnOnce(&mut Desktop<'_>, &egui::Context)) {
    with_state(name, |mut state, ctx| {
        let mut client = Client::new(ctx, &mut Options::default(), &crate::load_icon());
        test(&mut Desktop { runtime: &mut state, client: &mut client }, ctx);
        state.shutdown();
    });
}

pub(super) fn with_state(name: &str, test: impl FnOnce(State, &egui::Context)) {
    if std::env::var_os("ATOMICS_APP_TEST_CHILD").is_none() {
        let dir = crate::test_support::tempdir().unwrap();
        use std::os::unix::fs::DirBuilderExt;
        let runtime = dir.path().join("runtime");
        std::fs::DirBuilder::new().mode(0o700).create(&runtime).unwrap();
        let output = crate::test_support::output(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", name, "--nocapture"])
                .current_dir(dir.path())
                .env("ATOMICS_APP_TEST_CHILD", "1")
                .env("CODEX_HOME", dir.path().join("codex"))
                .env("CLAUDE_CONFIG_DIR", dir.path().join("claude"))
                .env("HOME", dir.path())
                .env("ATOMICS_HOME", dir.path().join("state"))
                .env("ATOMICS_CONFIG", dir.path().join("config.toml"))
                .env("ATOMICS_SOCKET", dir.path().join("atomics.sock"))
                .env("XDG_RUNTIME_DIR", runtime)
                .env_remove("ATOMICS_PANE"),
        );
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    // Agents run bare in tests unless a test sets up a fake sbx; State never reaches the real one.
    let config = paths::config_file();
    if !config.exists() {
        std::fs::write(&config, atomics_core::config::DEFAULT_CONFIG.replace("mode = \"sandbox\"", "mode = \"bare\""))
            .unwrap();
    }
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = State::new(
        &cc,
        Options { migration_errors: vec!["Migration failed".into()], ..Options::default() },
        &crate::load_icon(),
    );
    // Tests use an available but empty keychain. An unavailable helper now fails closed;
    // only tests of that failure should use a missing executable.
    let (_keychain_dir, keychain, _) = crate::credentials::tests::machine(cfg!(target_os = "macos"));
    app.relay = Arc::new(crate::relay::Service::new(paths::atomics_home(), keychain));
    // Most tests start from an idle fixture. Tests of asynchronous startup start their
    // own gated sweep after this initial completion, rather than racing fixture setup.
    if let Some(job) = app.startup_cleanup.take() {
        job.join().unwrap();
    }
    assert_eq!(app.toast.as_ref().unwrap().0, "Migration failed");
    test(app, &ctx);
}

#[test]
fn config_reloads_when_host_overrides_are_created_changed_or_removed() {
    with_state("app::tests::config_reloads_when_host_overrides_are_created_changed_or_removed", |mut state, ctx| {
        let [base, host_file] = Config::source_paths(&state.config_path);
        std::fs::write(&base, "[font]\nsize = 14.0\ndiff_size = 11.0").unwrap();
        state.config_checked = Instant::now() - Duration::from_secs(2);
        state.runtime_tick(ctx);
        assert_eq!(state.config.font.size, 14.0);

        std::fs::create_dir_all(host_file.parent().unwrap()).unwrap();
        for size in [16.0, 18.0] {
            std::fs::write(&host_file, format!("[font]\nsize = {size:.1}")).unwrap();
            // Same-length edits with identical mtimes must still reload.
            std::fs::File::options()
                .write(true)
                .open(&host_file)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1)))
                .unwrap();
            state.config_checked = Instant::now() - Duration::from_secs(2);
            state.runtime_tick(ctx);
            assert_eq!(state.config.font.size, size);
            assert_eq!(state.config.font.diff_size, 11.0);
        }
        std::fs::write(&host_file, "[font]\nsize = 'invalid'").unwrap();
        state.config_checked = Instant::now() - Duration::from_secs(2);
        state.runtime_tick(ctx);
        assert!(state.config_error.is_some());
        assert_eq!(state.config.font.size, 18.0);

        std::fs::remove_file(&host_file).unwrap();
        state.config_checked = Instant::now() - Duration::from_secs(2);
        state.runtime_tick(ctx);
        assert!(state.config_error.is_none());
        assert_eq!(state.config.font.size, 14.0);
    });
}

#[test]
fn themes_reload_live_and_terminals_answer_the_new_colors() {
    with_state("app::tests::themes_reload_live_and_terminals_answer_the_new_colors", |mut state, ctx| {
        let reload = |state: &mut State| {
            state.config_checked = Instant::now() - Duration::from_secs(2);
            state.runtime_tick(ctx);
        };
        let wait_for = |terminal: &Terminal, text: &str| {
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
            while !terminal.screen_text(24).contains(text) {
                assert!(Instant::now() < deadline, "terminal never showed {text}:\n{}", terminal.screen_text(24));
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let color = |hex: &str| crate::theme::parse_hex(hex).unwrap();
        let themes = theme::themes_dir(&state.config_path);
        std::fs::create_dir_all(&themes).unwrap();
        std::fs::write(themes.join("mine.toml"), "extends = \"midnight\"\nterminal = \"#102030\"").unwrap();
        std::fs::write(&state.config_path, "[theme]\nname = \"mine\"\naccent = \"#405060\"").unwrap();
        reload(&mut state);
        assert!(state.config_error.is_none() && state.theme_error.is_none(), "{:?}", state.theme_error);
        assert_eq!(state.palette.terminal, color("#102030"));
        assert_eq!(state.palette.accent, color("#405060"), "inline [theme] tokens win");
        assert_eq!(state.palette.syntax, "OneHalfDark", "the rest comes from midnight");

        let terminal = Terminal::spawn(
            state.wake.clone(),
            state.palette.terminal_colors(),
            "/bin/sh".into(),
            crate::terminal::color_query_args("\\033]11;?\\007", 24),
            &state.launch_dir,
            HashMap::new(),
            100,
        )
        .unwrap();
        wait_for(&terminal, "ready");
        terminal.write(b"x".to_vec());
        wait_for(&terminal, "^]11;rgb:1010/2020/3030!");
        let mut shell = session("shell", AgentKind::Shell, AgentStatus::Idle, &state.launch_dir);
        shell.terminal = Some(terminal);
        state.sessions = vec![shell];

        // Same length, so only the content tells the edit apart.
        std::fs::write(themes.join("mine.toml"), "extends = \"midnight\"\nterminal = \"#a0b0c0\"").unwrap();
        reload(&mut state);
        assert_eq!(state.toast.as_ref().unwrap().0, "Theme reloaded");
        assert_eq!(state.palette.terminal, color("#a0b0c0"));
        let terminal = state.sessions[0].terminal.as_ref().unwrap();
        terminal.write(b"x".to_vec());
        wait_for(terminal, "^]11;rgb:a0a0/b0b0/c0c0!");

        std::fs::write(themes.join("mine.toml"), "extends = \"midnight\"\nterminal = \"#a0b0c\"").unwrap();
        reload(&mut state);
        let error = state.theme_error.clone().unwrap();
        assert!(error.contains("themes/mine.toml: terminal = \"#a0b0c\""), "{error}");
        assert!(state.config_error.is_none(), "the config itself still loads");
        assert_eq!(state.palette.terminal, color("#16181d"), "a bad token falls back to the theme it extends");
    });
}

#[test]
fn review_followups() {
    with_app("app::tests::review_followups", |app, ctx| {
        permission_hooks_wake_waiters_and_accept_decisions(app, ctx);
        busy_text_is_only_a_fallback(app, ctx);
        close_idle_leaves_shells_alone(app);
        voice_to_a_shell_stays_in_the_steer_bar(app, ctx);
        plan_set_resolves_cached_worktree_projects(app);
        dispatch_requires_integrated_dependencies(app);
    });
}

pub(super) fn request(app: &mut Desktop<'_>, value: serde_json::Value) -> ControlResponse {
    let (tx, rx) = channel();
    app.runtime.control(&app.runtime.ctx.clone(), serde_json::from_value(value).unwrap(), tx);
    match rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(2))).unwrap() {
        ControlOutcome::Done(response) => response,
        _ => panic!("unexpected deferred response"),
    }
}

pub(super) fn repository() -> (crate::test_support::TempDir, PathBuf) {
    let dir = crate::test_support::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git::git(&repo, &["init", "-q", "-b", "main"]).unwrap();
    git::git(&repo, &["config", "user.name", "Test"]).unwrap();
    git::git(&repo, &["config", "user.email", "test@example.org"]).unwrap();
    git::git(&repo, &["commit", "--allow-empty", "-qm", "initial"]).unwrap();
    (dir, repo)
}

fn permission_hooks_wake_waiters_and_accept_decisions(app: &mut Desktop<'_>, ctx: &egui::Context) {
    let dir = crate::test_support::tempdir().unwrap();
    app.runtime.sessions = vec![session("codex", AgentKind::Generic, AgentStatus::Working, dir.path())];
    app.runtime.sessions[0].claude_id = Some("s1".into());
    let permission = |command: &str, reason: &str| {
        let response = Arc::new(ipc::ApprovalReply::default());
        (
            HookMessage {
                pane: Some("codex".into()),
                ancestors: Vec::new(),
                event: atomics_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name":"PermissionRequest", "session_id":"s1", "cwd":dir.path(),
                    "tool_name":"Bash", "tool_input":{"command":command,"description":reason}
                }))
                .unwrap(),
                delivered: 0,
                approval_reply: Some(response.clone()),
            },
            response,
        )
    };
    for decision in [Decision::Allow, Decision::Deny, Decision::AllowAlways] {
        let (wait_tx, wait_rx) = channel();
        app.runtime.control(
            ctx,
            ControlRequest::Wait {
                sessions: vec!["codex".into()],
                statuses: vec!["needs you".into()],
                after: Some(app.runtime.event_seq),
                timeout_secs: Some(30),
            },
            wait_tx,
        );
        assert!(wait_rx.try_recv().is_err());
        let (hook, hook_rx) = permission("touch approval-file", "Create a file");
        app.runtime.on_hook(hook);
        assert_eq!(app.runtime.sessions[0].status, AgentStatus::NeedsInput);
        assert!(app.runtime.sessions[0].hooks_active);
        let ControlOutcome::Done(response) = wait_rx.recv().unwrap() else { panic!("wait response") };
        assert_eq!(response.result["events"][0]["approval"]["command"], "touch approval-file");
        assert_eq!(response.result["events"][0]["approval"]["reason"], "Create a file");
        assert_eq!(response.result["events"][0]["approval"]["cwd"], json!(dir.path()));
        let (tx, rx) = channel();
        app.runtime.control(
            ctx,
            ControlRequest::Approve { session: "codex".into(), decision, message: Some("Reviewed".into()) },
            tx,
        );
        assert!(matches!(rx.recv().unwrap(), ControlOutcome::Done(response) if response.ok));
        let output: serde_json::Value = serde_json::from_str(&hook_rx.wait(Duration::ZERO).stdout.unwrap()).unwrap();
        assert_eq!(
            output["hookSpecificOutput"]["decision"]["behavior"],
            if decision == Decision::Deny { "deny" } else { "allow" }
        );
        assert!(app.runtime.sessions[0].approvals.is_empty());
    }
    let (hook, rx) = permission("touch approval-file", "A different reason");
    app.runtime.on_hook(hook);
    assert!(rx.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
    assert!(app.runtime.sessions[0].approvals.is_empty());
    let (hook, rx) = permission("git status", "Check code");
    app.runtime.on_hook(hook);
    assert!(rx.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
    let (hook, rx) = permission("touch another-file", "Another request");
    app.runtime.on_hook(hook);
    assert!(rx.wait(Duration::ZERO).stdout.is_none());
    assert!(
        app.runtime
            .approve(
                0,
                &app.runtime.sessions[0].approvals.first().map(|pending| pending.id.clone()).unwrap_or_default(),
                Decision::Allow,
                None
            )
            .is_err()
    );
    assert_eq!(app.runtime.sessions[0].approvals.len(), 1);
    let event = atomics_core::hook::HookEvent::from_payload(&json!({
        "hook_event_name":"PostToolUse", "session_id":"s1", "cwd":dir.path(),
        "tool_name":"Bash", "tool_input":{"command":"touch another-file"}
    }))
    .unwrap();
    app.runtime.on_hook(HookMessage {
        pane: Some("codex".into()),
        ancestors: Vec::new(),
        event,
        delivered: 0,
        approval_reply: None,
    });
    assert!(app.runtime.sessions[0].approvals.is_empty());
    assert_eq!(app.runtime.sessions[0].status, AgentStatus::Working);
    assert!(
        app.runtime
            .approve(
                0,
                &app.runtime.sessions[0].approvals.first().map(|pending| pending.id.clone()).unwrap_or_default(),
                Decision::Deny,
                None
            )
            .is_err()
    );
    assert!(CHIEF_ALLOWED_TOOLS.contains(&"mcp__atomics__atomics_approve"));
}

fn busy_text_is_only_a_fallback(app: &mut Desktop<'_>, _ctx: &egui::Context) {
    app.runtime.config.agents.push(atomics_core::config::AgentConfig {
        name: "test".into(),
        command: vec![],
        kind: AgentKind::Generic,
        busy_text: Some("esc to interrupt".into()),
        isolation: None,
    });
    let dir = crate::test_support::tempdir().unwrap();
    let terminal = Terminal::spawn(
        app.runtime.wake.clone(),
        app.runtime.palette.terminal_colors(),
        "/bin/sh".into(),
        vec!["-c".into(), "printf 'esc to interrupt'; sleep 10".into()],
        dir.path(),
        HashMap::new(),
        100,
    )
    .unwrap();
    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(2));
    while !terminal.screen_text(12).contains("esc to interrupt") {
        assert!(Instant::now() < deadline, "terminal did not produce busy text");
        std::thread::sleep(Duration::from_millis(10));
    }
    let process_id = terminal.process_id();
    app.runtime.sessions = vec![
        session("fallback", AgentKind::Generic, AgentStatus::Idle, dir.path()),
        session("same-cwd", AgentKind::Generic, AgentStatus::Idle, dir.path()),
    ];
    app.runtime.sessions[0].terminal = Some(terminal);
    app.runtime.busy_checked = Instant::now() - Duration::from_secs(1);
    app.runtime.track_busy();
    assert_eq!(app.runtime.sessions[0].status, AgentStatus::Working);
    let event = atomics_core::hook::HookEvent::from_payload(&json!({
        "hook_event_name":"SessionStart", "session_id":"mapped-codex-session", "cwd":dir.path(),
    }))
    .unwrap();
    app.runtime.on_hook(HookMessage {
        pane: None,
        ancestors: vec![process_id],
        event,
        delivered: 0,
        approval_reply: None,
    });
    assert_eq!(app.runtime.sessions.len(), 2);
    assert_eq!(app.runtime.sessions[0].claude_id.as_deref(), Some("mapped-codex-session"));
    assert_eq!(app.runtime.sessions[1].claude_id.as_deref(), Some("same-cwd"));
    app.runtime.sessions[0].status = AgentStatus::Done;
    app.runtime.busy_checked = Instant::now() - Duration::from_secs(1);
    app.runtime.track_busy();
    assert_eq!(app.runtime.sessions[0].status, AgentStatus::Done);
    let event = atomics_core::hook::HookEvent::from_payload(&json!({
        "hook_event_name":"PermissionRequest", "session_id":"mapped-codex-session", "cwd":dir.path(),
        "tool_name":"Bash", "tool_input":{"command":"touch fixture", "description":"test"}
    }))
    .unwrap();
    let response = Arc::new(ipc::ApprovalReply::default());
    app.runtime.on_hook(HookMessage {
        pane: None,
        ancestors: vec![],
        event,
        delivered: 0,
        approval_reply: Some(response),
    });
    assert_eq!(app.runtime.sessions.len(), 2);
    assert_eq!(app.runtime.sessions[0].status, AgentStatus::NeedsInput);
    let before = app.runtime.sessions[0].terminal.as_ref().unwrap().screen_text(12);
    app.runtime.sessions[0].pending = Some(Pending {
        text: "previous steering message".into(),
        sent: Instant::now() - DELIVERY_TIMEOUT,
        attempts: 1,
        was_working: true,
        codex: false,
        screen_before: String::new(),
        paste_seen: false,
        reply: None,
    });
    app.runtime.verify_deliveries();
    assert_eq!(app.runtime.sessions[0].pending.as_ref().unwrap().attempts, 1);
    let response = request(app, json!({"cmd":"send", "session":"fallback", "text":"held steering message"}));
    assert_eq!(response.result["held"], true);
    assert_eq!(response.result["sent"], false);
    let rejected = request(app, json!({"cmd":"send", "session":"fallback", "text":"y", "raw":true}));
    assert!(!rejected.ok);
    assert!(rejected.error.unwrap().contains("atomics_approve"));
    assert_eq!(app.runtime.sessions[0].held, ["held steering message"]);
    app.runtime.active = Some("fallback".into());
    app.client.steer_text = "steer while waiting".into();
    app.send_steer();
    app.runtime.drain_actions();
    assert!(app.runtime.toast.as_ref().unwrap().0.starts_with("Held for 1"));
    let root = app.runtime.sessions[0].root();
    app.runtime.diffs.entry(root.clone()).or_default().comments.push(steer::ReviewComment {
        path: "fixture.rs".into(),
        line: Some(1),
        removed: false,
        excerpt: "line".into(),
        body: "review while waiting".into(),
    });
    let review = request(app, json!({"cmd":"send_review", "session":"fallback"}));
    assert_eq!(review.result["held"], true);
    assert_eq!(review.result["held_comments"], 1);
    assert!(review.result.get("sent_comments").is_none());
    assert!(app.runtime.toast.as_ref().unwrap().0.starts_with("Held 1 comment"));
    assert_eq!(app.runtime.sessions[0].held.len(), 3);

    assert_eq!(app.runtime.sessions[0].terminal.as_ref().unwrap().screen_text(12), before);
    app.runtime
        .approve(
            0,
            &app.runtime.sessions[0].approvals.first().map(|pending| pending.id.clone()).unwrap_or_default(),
            Decision::Allow,
            None,
        )
        .unwrap();
    assert_eq!(app.runtime.sessions[0].held.len(), 3);
    assert_eq!(app.runtime.sessions[0].terminal.as_ref().unwrap().screen_text(12), before);
    let event = atomics_core::hook::HookEvent::from_payload(&json!({
        "hook_event_name":"PostToolUse", "session_id":"mapped-codex-session", "cwd":dir.path(),
        "tool_name":"Bash", "tool_input":{"command":"touch fixture"}
    }))
    .unwrap();
    app.runtime.on_hook(HookMessage { pane: None, ancestors: vec![], event, delivered: 0, approval_reply: None });
    assert!(app.runtime.sessions[0].held.is_empty());
    let delivered = &app.runtime.sessions[0].pending.as_ref().unwrap().text;
    assert!(delivered.contains("held steering message"));
    assert!(delivered.contains("steer while waiting"));
    assert!(delivered.contains("review while waiting"));
    app.runtime.sessions.clear();
}

fn close_idle_leaves_shells_alone(app: &mut Desktop<'_>) {
    let project = Path::new("/project");
    app.runtime.repos = vec![project.into()];
    app.runtime.sessions = vec![
        session("shell", AgentKind::Shell, AgentStatus::Idle, project),
        session("exited-shell", AgentKind::Shell, AgentStatus::Exited, project),
        session("idle", AgentKind::Generic, AgentStatus::Idle, project),
        session("exited", AgentKind::Claude, AgentStatus::Exited, project),
        session("working", AgentKind::Generic, AgentStatus::Working, project),
        session("done", AgentKind::Claude, AgentStatus::Done, project),
        session("chief", AgentKind::Claude, AgentStatus::Idle, project),
        session("in-plan", AgentKind::Claude, AgentStatus::Idle, project),
        session("elsewhere", AgentKind::Claude, AgentStatus::Idle, Path::new("/other")),
    ];
    app.runtime.sessions[6].chief = true;
    app.runtime.plans[app.runtime.selected_plan].tasks =
        vec![Task { session: Some("in-plan".into()), ..Task::default() }];
    assert_eq!(app.runtime.close_idle(project), 2);
    assert_eq!(
        app.runtime.sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        ["shell", "exited-shell", "working", "done", "chief", "in-plan", "elsewhere"]
    );
}

fn voice_to_a_shell_stays_in_the_steer_bar(app: &mut Desktop<'_>, ctx: &egui::Context) {
    app.runtime.config.voice.target = VoiceTarget::Active;
    app.runtime.active = Some("shell".into());
    assert!(app.runtime.sessions[0].can_receive());
    app.client.steer_text = "existing draft".into();
    app.send_voice(ctx, "delete everything");
    assert_eq!(app.client.steer_text, "existing draft\ndelete everything");
    assert!(app.client.focus_steer);
    assert!(app.runtime.sessions[0].pending.is_none());
}

fn plan_set_resolves_cached_worktree_projects(app: &mut Desktop<'_>) {
    let project = PathBuf::from("/project");
    app.runtime.worktree_project.insert("/worktree".into(), project.clone());
    let (tx, rx) = channel();
    app.runtime.control(
        &app.runtime.ctx.clone(),
        ControlRequest::PlanSet {
            ticket: None,
            plan: None,
            goal: "ship".into(),
            tasks: vec![Task { id: "a".into(), repo: Some("/worktree/src".into()), ..Task::default() }],
            integration: None,
        },
        tx,
    );
    assert!(matches!(rx.recv().unwrap(), ControlOutcome::Done(response) if response.ok));
    assert_eq!(app.runtime.plans[app.runtime.selected_plan].project, Some(project));
}

fn dispatch_requires_integrated_dependencies(app: &mut Desktop<'_>) {
    let plan_id = app.runtime.plans[app.runtime.selected_plan].id.clone();
    app.runtime.plans[app.runtime.selected_plan] = Plan {
        id: plan_id,
        approved: true,
        tasks: vec![
            Task { id: "a".into(), branch: Some("task-a".into()), status: TaskStatus::Done, ..Task::default() },
            Task { id: "b".into(), depends_on: vec!["a".into()], ..Task::default() },
        ],
        ..Plan::default()
    };
    for only in [Some("b"), None] {
        let error = app.runtime.dispatch(app.runtime.selected_plan, only).unwrap_err();
        assert!(error.contains("atomics_plan_integrate"), "{error}");
        assert!(error.contains("a"));
        assert!(app.runtime.creating.is_empty());
    }
    let into = app.runtime.plans[app.runtime.selected_plan].integration_branch();
    for result in [
        Err("failed".into()),
        Ok(Integration {
            committed: false,
            merge: Merge::Conflict(vec!["file".into()]),
            worktree: "/integration".into(),
        }),
    ] {
        app.runtime.finish_integration(Integrated {
            commit: None,
            plan: app.runtime.plans[app.runtime.selected_plan].id.clone(),
            task: "a".into(),
            branch: "task-a".into(),
            into: into.clone(),
            result,
            reply: None,
        });
        assert!(app.runtime.plans[app.runtime.selected_plan].tasks[0].integrated.is_none());
        assert!(app.runtime.plans[app.runtime.selected_plan].ready().is_empty());
    }
    app.runtime.plans[app.runtime.selected_plan].tasks[0].status = TaskStatus::Done;
    app.runtime.finish_integration(Integrated {
        commit: None,
        plan: app.runtime.plans[app.runtime.selected_plan].id.clone(),
        task: "a".into(),
        branch: "task-a".into(),
        into: into.clone(),
        result: Ok(Integration { committed: true, merge: Merge::Merged, worktree: "/integration".into() }),
        reply: None,
    });
    assert_eq!(app.runtime.plans[app.runtime.selected_plan].tasks[0].integrated, Some(into));
    assert_eq!(
        app.runtime.plans[app.runtime.selected_plan].ready().iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
        ["b"]
    );
    let loaded = Plan::load(
        &paths::atomics_home().join("plans").join(format!("{}.json", app.runtime.plans[app.runtime.selected_plan].id)),
    )
    .unwrap();
    assert_eq!(loaded.tasks[0].integrated, app.runtime.plans[app.runtime.selected_plan].tasks[0].integrated);
}
