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
        first_run_hint: None,
        activity: heartbeat::Activity::default(),
        approvals: Vec::new(),
        allowed: Vec::new(),
        held: Vec::new(),
        approval_hold: false,
        menu_absent_since: None,
        answered_menu: None,
        chief: false,
        pending: None,
        sandbox: None,
        cache_env: Vec::new(),
        relay: None,
        screen_busy: false,
        busy_absent_since: None,
        ref_guard: None,
        scratch: None,
        naming: Naming::Folder,
        mcp_needed: Vec::new(),
        warning: None,
    }
}

/// Set by a test of a first run: its home has no config, as a person's has.
pub(super) const FRESH_HOME: &str = "ATOMICS_TEST_FRESH_HOME";

pub(super) fn with_app(name: &str, test: impl FnOnce(&mut Desktop<'_>, &egui::Context)) {
    with_state(name, |mut state, ctx| {
        let mut client = Client::new(ctx, &mut Options::default(), &crate::load_icon());
        test(&mut Desktop { runtime: &mut state, client: &mut client }, ctx);
        state.shutdown();
    });
}

pub(super) fn with_app_env(
    name: &str,
    env: impl FnOnce(&Path) -> Vec<(&'static str, std::ffi::OsString)>,
    test: impl FnOnce(&mut Desktop<'_>, &egui::Context),
) {
    with_login_state(name, env, |mut state, ctx| {
        let mut client = Client::new(ctx, &mut Options::default(), &crate::load_icon());
        test(&mut Desktop { runtime: &mut state, client: &mut client }, ctx);
        state.shutdown();
    });
}

pub(super) fn fixture_agent(name: &str) -> PathBuf {
    let bin = paths::home().join("test-agents");
    std::fs::create_dir_all(&bin).unwrap();
    let program = bin.join(name);
    if !program.exists() {
        crate::test_support::copy_program(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name),
            &program,
        );
    }
    program
}

#[test]
fn app_fixtures_cannot_find_installed_cloud_agent_or_vm_programs() {
    with_state("app::tests::app_fixtures_cannot_find_installed_cloud_agent_or_vm_programs", |_state, _| {
        for name in ["az", "gh", "sbx", "claude", "codex", "limactl"] {
            assert!(crate::setup::find_program(name).is_none(), "{name} must require an explicit fixture");
        }
        let home = paths::home();
        for name in ["AZURE_CONFIG_DIR", "GH_CONFIG_DIR", "LIMA_HOME", "CODEX_HOME", "CLAUDE_CONFIG_DIR"] {
            assert!(PathBuf::from(std::env::var_os(name).unwrap()).starts_with(&home), "{name} belongs to this test");
        }
    });
}

pub(super) fn with_state(name: &str, test: impl FnOnce(State, &egui::Context)) {
    run_state(name, None::<fn(&Path) -> Vec<(&'static str, std::ffi::OsString)>>, test);
}

/// Keep asynchronous fixtures' settings consistent with what a runtime reload reads.
pub(super) fn persist_config(state: &mut State) {
    std::fs::write(&state.config_path, toml::to_string(&state.config).unwrap()).unwrap();
    state.reload_config();
    assert!(state.config_error.is_none(), "fixture config did not reload: {:?}", state.config_error);
}

/// [`with_state`] in a child that also has the variables `env` gives, which is called in the
/// parent with the child's home folder. The child takes on its login shell's PATH before the
/// app starts, as `main` does.
pub(super) fn with_login_state(
    name: &str,
    env: impl FnOnce(&Path) -> Vec<(&'static str, std::ffi::OsString)>,
    test: impl FnOnce(State, &egui::Context),
) {
    run_state(name, Some(env), test);
}

fn run_state(
    name: &str,
    env: Option<impl FnOnce(&Path) -> Vec<(&'static str, std::ffi::OsString)>>,
    test: impl FnOnce(State, &egui::Context),
) {
    if std::env::var_os("ATOMICS_APP_TEST_CHILD").is_none() {
        let dir = crate::test_support::tempdir().unwrap();
        use std::os::unix::fs::DirBuilderExt;
        let runtime = dir.path().join("runtime");
        std::fs::DirBuilder::new().mode(0o700).create(&runtime).unwrap();
        // Not inside `dir`, the child's home and config folder: tests scan that for what the app wrote,
        // and the child's fixtures may hold the secrets they look for.
        let folders = crate::test_support::tempdir().unwrap();
        let shell = dir.path().join("test-shell");
        crate::test_support::login_shell(&shell);
        let output = crate::test_support::output(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", name, "--nocapture"])
                .current_dir(dir.path())
                .env("ATOMICS_APP_TEST_CHILD", "1")
                .env(crate::test_support::CHILD_FOLDERS, folders.path())
                .env("CODEX_HOME", dir.path().join("codex"))
                .env("CLAUDE_CONFIG_DIR", dir.path().join("claude"))
                .env("HOME", dir.path())
                .env("ATOMICS_HOME", dir.path().join("state"))
                .env("ATOMICS_CONFIG", dir.path().join("config.toml"))
                .env("ATOMICS_SOCKET", dir.path().join("atomics.sock"))
                .env("XDG_RUNTIME_DIR", runtime)
                .env("SHELL", shell)
                .env_remove("BASH_ENV")
                .env_remove("ENV")
                .env_remove("ATOMICS_PANE")
                .envs(env.map(|env| env(dir.path())).unwrap_or_default()),
        );
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    if env.is_some() {
        // SAFETY: the test harness's own threads only wait for this test; nothing reads the
        // environment while it changes.
        unsafe { crate::setup::adopt_login_path() };
    }
    if std::env::var_os(FRESH_HOME).is_some() {
        // SAFETY: as above. The config goes where a person's does, by the folder this home's XDG variable names.
        unsafe { std::env::remove_var("ATOMICS_CONFIG") };
    }
    // Agents run bare in tests unless a test sets up a fake sbx; State never reaches the real one.
    let config = paths::config_file();
    if !config.exists() && std::env::var_os(FRESH_HOME).is_none() {
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
    if std::env::var_os("ATOMICS_TEST_PIN_ERROR").is_some() {
        assert!(app.toast.as_ref().unwrap().0.starts_with("Migration failed\nUnreadable sandbox git pin "));
    } else {
        assert_eq!(app.toast.as_ref().unwrap().0, "Migration failed");
    }
    test(app, &ctx);
    crate::test_support::assert_no_real_programs();
}

#[test]
fn osc52_only_copies_from_the_focused_bare_tile_and_names_it() {
    fn frame(
        app: &mut Desktop<'_>,
        ctx: &egui::Context,
        focus: usize,
        visible: &[usize],
        window_focused: bool,
    ) -> Vec<String> {
        let output = ctx.run_ui(
            egui::RawInput {
                focused: window_focused,
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(900.0, 700.0))),
                ..Default::default()
            },
            |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.memory_mut(|m| m.request_focus(egui::Id::NULL.with(("clipboard-tile", focus))));
                    let p = app.palette();
                    let metrics = Metrics::new(ui, &p, 13.0);
                    for &index in visible {
                        let id = egui::Id::NULL.with(("clipboard-tile", index));
                        let rect = Rect::from_min_size(pos2(10.0, 10.0 + index as f32 * 300.0), vec2(850.0, 280.0));
                        let terminal = app.runtime.sessions[index].terminal.as_ref().unwrap();
                        app.client.terminal_views.entry(id).or_default().show(terminal, ui, rect, id, &metrics, &p);
                    }
                    app.terminal_clipboard(ctx);
                });
            },
        );
        let copies = output
            .platform_output
            .commands
            .iter()
            .filter_map(|command| match command {
                egui::OutputCommand::CopyText(text) => Some(text.clone()),
                _ => None,
            })
            .collect();
        output.drop_without_applying_deltas();
        copies
    }

    fn emit(app: &Desktop<'_>, index: usize) {
        let terminal = app.runtime.sessions[index].terminal.as_ref().unwrap();
        terminal.shared.title.lock().unwrap().take();
        terminal.write(b"copy\n".to_vec());
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
        while terminal.shared.title.lock().unwrap().as_deref() != Some("copied") {
            assert!(Instant::now() < deadline, "terminal did not emit OSC 52");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    with_app("app::tests::osc52_only_copies_from_the_focused_bare_tile_and_names_it", |app, ctx| {
        app.runtime.sessions.clear();
        for id in ["tile-a", "tile-b"] {
            let mut tile = session(id, AgentKind::Shell, AgentStatus::Idle, &app.runtime.launch_dir);
            tile.terminal = Some(
                Terminal::spawn(
                    app.runtime.wake.clone(),
                    app.runtime.theme.terminal_colors(),
                    "/bin/sh".into(),
                    vec![
                        "-c".into(),
                        "while IFS= read -r line; do printf '\\033]52;c;Y2xpcGJvYXJk\\007\\033]2;copied\\007'; done"
                            .into(),
                    ],
                    &app.runtime.launch_dir,
                    HashMap::new(),
                    100,
                )
                .unwrap(),
            );
            app.runtime.sessions.push(tile);
        }
        // The active session alone is insufficient: the actual keyboard focus determines access.
        app.runtime.active = Some("tile-a".into());
        assert!(frame(app, ctx, 1, &[0, 1], true).is_empty());
        emit(app, 0);
        emit(app, 1);
        assert_eq!(frame(app, ctx, 1, &[0, 1], true), ["clipboard"]);
        assert_eq!(app.runtime.toast.as_ref().unwrap().0, "Clipboard set by tile-b (tile-b)");
        assert!(frame(app, ctx, 0, &[0, 1], true).is_empty(), "background writes cannot be replayed");

        emit(app, 0);
        assert!(frame(app, ctx, 1, &[0, 1], true).is_empty(), "losing focus drops a pending copy");
        emit(app, 1);
        app.runtime.config.terminal.clipboard = atomics_core::config::ClipboardPolicy::Off;
        assert!(frame(app, ctx, 1, &[0, 1], true).is_empty(), "off discards pending copies");
        emit(app, 1);
        app.runtime.config.terminal.clipboard = atomics_core::config::ClipboardPolicy::Focused;
        assert!(frame(app, ctx, 1, &[0, 1], true).is_empty());

        app.runtime.sessions[1].sandbox = Some("sandboxed".into());
        assert!(frame(app, ctx, 1, &[0, 1], true).is_empty());
        emit(app, 1);
        assert!(frame(app, ctx, 1, &[0, 1], true).is_empty(), "focused sandboxes stay off");
        app.runtime.sessions[1].sandbox = None;
        assert!(frame(app, ctx, 1, &[0, 1], true).is_empty());
        emit(app, 1);
        assert!(frame(app, ctx, 1, &[0], true).is_empty(), "hidden tiles cannot copy through stale focus");
        assert!(frame(app, ctx, 1, &[0, 1], false).is_empty());
        emit(app, 1);
        assert!(frame(app, ctx, 1, &[0, 1], false).is_empty(), "an unfocused window cannot copy");
        assert!(frame(app, ctx, 1, &[0, 1], true).is_empty());
        std::thread::sleep(Duration::from_secs(1));
        emit(app, 1);
        assert_eq!(frame(app, ctx, 1, &[0, 1], true), ["clipboard"]);
    });
}

#[test]
fn session_ids_and_prefixes_cannot_be_shadowed_by_titles() {
    with_state("app::tests::session_ids_and_prefixes_cannot_be_shadowed_by_titles", |mut state, _ctx| {
        let mut attacker = session("other-id", AgentKind::Shell, AgentStatus::Idle, &state.launch_dir);
        attacker.title = "target-id".into();
        attacker.sandbox = Some("untrusted".into());
        state.sessions =
            vec![attacker, session("target-id-long", AgentKind::Shell, AgentStatus::Idle, &state.launch_dir)];
        assert_eq!(state.find("target-id"), Some(1), "a title cannot steal a unique id prefix");
        state.sessions[0].title = "target-id-long".into();
        assert_eq!(state.find("target-id-long"), Some(1), "exact ids take precedence too");
        state.sessions[0].title = "target".into();
        state.sessions.push(session("target-other", AgentKind::Shell, AgentStatus::Idle, &state.launch_dir));
        assert_eq!(state.find("target"), None, "ambiguous prefixes cannot fall back to titles");
        state.sessions[0].title = "Display name".into();
        assert_eq!(state.find(" display NAME "), Some(0));
        assert_eq!(state.find("  "), None);
        state.plans = vec![Plan {
            id: "lookup".into(),
            tasks: vec![Task {
                id: "t1".into(),
                title: "Fix the parser".into(),
                session: Some("other-id".into()),
                ..Task::default()
            }],
            ..Plan::default()
        }];
        assert_eq!(state.find(" T1 · FIX THE PARSER "), Some(0), "task labels are searchable");
        assert_eq!(state.find("Display name"), Some(0), "legacy titles still work");
        state.sessions.push(session(
            "t1 · Fix the parser-long",
            AgentKind::Shell,
            AgentStatus::Idle,
            &state.launch_dir,
        ));
        assert_eq!(state.find("t1 · Fix the parser"), Some(3), "task labels cannot steal an id prefix");
        state.sessions.push(session(
            "t1 · Fix the parser-other",
            AgentKind::Shell,
            AgentStatus::Idle,
            &state.launch_dir,
        ));
        assert_eq!(state.find("t1 · Fix the parser"), None, "ambiguous prefixes cannot fall back to task labels");
        state.sessions[0].sandbox = None;
    });
}

#[test]
fn p5_review_duplicate_task_labels_require_a_session_id() {
    with_state("app::tests::p5_review_duplicate_task_labels_require_a_session_id", |mut state, _| {
        let root = state.launch_dir.clone();
        state.sessions = ["first-session", "second-session"]
            .into_iter()
            .map(|id| session(id, AgentKind::Generic, AgentStatus::Idle, &root))
            .collect();
        state.plans = ["first", "second"]
            .into_iter()
            .zip(&state.sessions)
            .map(|(id, session)| Plan {
                id: id.into(),
                tasks: vec![Task {
                    id: "t1".into(),
                    title: "Fix the parser".into(),
                    session: Some(session.id.clone()),
                    ..Default::default()
                }],
                ..Default::default()
            })
            .collect();
        assert_eq!(state.find("t1 · Fix the parser"), None);
        assert_eq!(state.find("first-session"), Some(0));
        assert_eq!(state.find("second-"), Some(1));
        state.sessions[0].rename("Parser investigation");
        assert_eq!(state.find("Parser investigation"), Some(0));
        assert_eq!(state.find("t1 · Fix the parser"), Some(1));
    });
}

#[test]
fn config_reloads_when_host_overrides_are_created_changed_or_removed() {
    with_state("app::tests::config_reloads_when_host_overrides_are_created_changed_or_removed", |mut state, _ctx| {
        let [base, host_file] = Config::source_paths(&state.config_path);
        std::fs::write(&base, "[font]\nsize = 14.0\ndiff_size = 11.0").unwrap();
        state.config_checked = Instant::now() - Duration::from_secs(2);
        state.runtime_tick();
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
            state.runtime_tick();
            assert_eq!(state.config.font.size, size);
            assert_eq!(state.config.font.diff_size, 11.0);
        }
        std::fs::write(&host_file, "[font]\nsize = 'invalid'").unwrap();
        state.config_checked = Instant::now() - Duration::from_secs(2);
        state.runtime_tick();
        assert!(state.config_error.is_some());
        assert_eq!(state.config.font.size, 18.0);

        std::fs::remove_file(&host_file).unwrap();
        state.config_checked = Instant::now() - Duration::from_secs(2);
        state.runtime_tick();
        assert!(state.config_error.is_none());
        assert_eq!(state.config.font.size, 14.0);
    });
}

#[test]
fn themes_reload_live_and_terminals_answer_the_new_colors() {
    with_state("app::tests::themes_reload_live_and_terminals_answer_the_new_colors", |mut state, _ctx| {
        let reload = |state: &mut State| {
            state.config_checked = Instant::now() - Duration::from_secs(2);
            state.runtime_tick();
        };
        let wait_for = |terminal: &Terminal, text: &str| {
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
            while !terminal.screen_text(24).contains(text) {
                assert!(Instant::now() < deadline, "terminal never showed {text}:\n{}", terminal.screen_text(24));
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let color = |hex: &str| atomics_ui::theme::parse_hex(hex).unwrap();
        let themes = theme::themes_dir(&state.config_path);
        std::fs::create_dir_all(&themes).unwrap();
        std::fs::write(themes.join("mine.toml"), "extends = \"midnight\"\nterminal = \"#102030\"").unwrap();
        std::fs::write(&state.config_path, "[theme]\nname = \"mine\"\naccent = \"#405060\"").unwrap();
        reload(&mut state);
        assert!(state.config_error.is_none() && state.theme_error.is_none(), "{:?}", state.theme_error);
        assert_eq!(state.theme.palette().terminal, color("#102030"));
        assert_eq!(state.theme.palette().accent, color("#405060"), "inline [theme] tokens win");
        assert_eq!(state.theme.syntax, "OneHalfDark", "the rest comes from midnight");

        let terminal = Terminal::spawn(
            state.wake.clone(),
            state.theme.terminal_colors(),
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
        assert_eq!(state.theme.palette().terminal, color("#a0b0c0"));
        let terminal = state.sessions[0].terminal.as_ref().unwrap();
        terminal.write(b"x".to_vec());
        wait_for(terminal, "^]11;rgb:a0a0/b0b0/c0c0!");

        std::fs::write(themes.join("mine.toml"), "extends = \"midnight\"\nterminal = \"#a0b0c\"").unwrap();
        reload(&mut state);
        let error = state.theme_error.clone().unwrap();
        assert!(error.contains("themes/mine.toml: terminal = \"#a0b0c\""), "{error}");
        assert!(state.config_error.is_none(), "the config itself still loads");
        assert_eq!(state.theme.palette().terminal, color("#16181d"), "a bad token falls back to the theme it extends");
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

#[test]
fn a_review_goes_to_an_agent_and_never_into_a_shell() {
    with_state("app::tests::a_review_goes_to_an_agent_and_never_into_a_shell", |mut state, _ctx| {
        let root = state.launch_dir.clone();
        let elsewhere = root.join("elsewhere");
        let agent = || session("agent", AgentKind::Claude, AgentStatus::Idle, &root);
        let shell = || session("shell", AgentKind::Shell, AgentStatus::Idle, &root);
        let other = || session("other", AgentKind::Claude, AgentStatus::Idle, &elsewhere);
        let comment = steer::ReviewComment {
            path: "a.rs".into(),
            line: Some(1),
            removed: false,
            excerpt: "line".into(),
            body: "Why?".into(),
        };
        // Who gets the review of `root`, with the computer's window on `active`.
        let mut review = |sessions: Vec<Session>, active: &str| {
            state.sessions = sessions;
            state.active = Some(active.into());
            state.diffs.entry(root.clone()).or_default().comments = vec![comment.clone()];
            state.hub.lock().unwrap().outbox.clear();
            let answer = state.deliver_review(&root);
            let delivered: Vec<String> = state.hub.lock().unwrap().outbox.keys().cloned().collect();
            let kept = state.diffs[&root].comments.len();
            (answer, delivered, kept)
        };
        let to_the_agent = |(answer, delivered, kept): (Result<serde_json::Value, String>, Vec<String>, usize)| {
            assert_eq!(answer.unwrap()["sent_comments"], 1);
            assert_eq!(delivered, ["agent"]);
            assert_eq!(kept, 0);
        };
        let refused = |(answer, delivered, kept): (Result<serde_json::Value, String>, Vec<String>, usize),
                       why: &str| {
            assert_eq!(answer.unwrap_err(), why, "what the browser is told");
            assert!(delivered.is_empty(), "{delivered:?}");
            assert_eq!(kept, 1, "the comments stay");
        };
        let no_agent = "No agent is working in this worktree";

        to_the_agent(review(vec![agent(), shell()], "shell"));
        refused(review(vec![shell()], "shell"), no_agent);
        refused(review(vec![shell(), other()], "other"), no_agent);
        to_the_agent(review(vec![agent(), shell(), other()], "other"));
        to_the_agent(review(vec![agent(), shell()], "agent"));

        // There is an agent, but the message has no way to it: the general words.
        let mut unreachable = agent();
        unreachable.claude_id = None;
        refused(review(vec![unreachable], "agent"), "could not deliver the review");

        // Through the mutation a client sends: the same refusal, with its reason, and the comments stay.
        state.sessions = vec![shell()];
        state.active = Some("shell".into());
        let worktree = root.to_string_lossy().into_owned();
        state.diffs.entry(root.clone()).or_default().prepared =
            Some(std::sync::Arc::new(atomics_core::diff_data::Prepared {
                diff: atomics_core::diff::WorktreeDiff {
                    base_ref: "main".into(),
                    base: "abc".into(),
                    files: Vec::new(),
                    partial: false,
                },
                highlights: Vec::new(),
                longest_line: 1,
                additions: 0,
                deletions: 0,
                all_files: Default::default(),
            }));
        state.diffs.entry(root.clone()).or_default().comments = vec![comment.clone()];
        state.hub.lock().unwrap().outbox.clear();
        let outcome = state.mutate_desktop(|op| {
            atomics_core::ops::Command::ReviewCommand(atomics_core::ops::ReviewCommand {
                op,
                worktree,
                action: atomics_core::ops::ReviewAction::Send,
            })
        });
        assert!(
            matches!(&outcome, atomics_core::ops::Outcome::Refused { message, .. }
                if message == no_agent),
            "{outcome:?}"
        );
        assert_eq!(state.diffs[&root].comments.len(), 1);
        assert!(state.hub.lock().unwrap().outbox.is_empty());
    });
}

#[test]
fn the_steer_bar_offers_all_agents_and_leaves_shells_out() {
    with_app("app::tests::the_steer_bar_offers_all_agents_and_leaves_shells_out", |app, ctx| {
        let dir = app.runtime.launch_dir.clone();
        app.runtime.sessions = vec![
            session("agent", AgentKind::Generic, AgentStatus::Idle, &dir),
            session("shell", AgentKind::Shell, AgentStatus::Idle, &dir),
        ];
        let frame = |app: &mut Desktop<'_>, events| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0))),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                ui.add_space(200.0);
                app.draw(|backend, client| atomics_ui::chief::steer_bar(ui, backend, client));
            });
            output.textures_delta.clear();
            let text = output.shapes.iter().filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some((text.galley.text().to_string(), text.visual_bounding_rect())),
                _ => None,
            });
            text.collect::<Vec<_>>()
        };
        frame(app, Vec::new());
        let shown = frame(app, Vec::new());
        let target = shown.iter().find(|(text, _)| text.starts_with("To: ")).expect("the target").1.center();
        let button = |pressed| egui::Event::PointerButton {
            pos: target,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(app, vec![egui::Event::PointerMoved(target), button(true), button(false)]);
        let list = frame(app, Vec::new());
        assert!(list.iter().any(|(text, _)| text == "All agents (1)"), "{list:?}");

        app.client.steer_target = SteerTarget::Agents;
        app.client.steer_text = "go on".into();
        let commands = app.draw(|backend, client| {
            atomics_ui::chief::send_steer(backend, client);
            std::mem::take(&mut client.commands)
        });
        assert_eq!(commands, [Command::Steer { sessions: vec!["agent".into()], text: "go on".into() }]);
    });
}

pub(super) fn request(app: &mut Desktop<'_>, value: serde_json::Value) -> ControlResponse {
    let (tx, rx) = channel();
    app.runtime.control(serde_json::from_value(value).unwrap(), tx);
    match rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(2))).unwrap() {
        ControlOutcome::Done(response) => response,
        _ => panic!("unexpected deferred response"),
    }
}

/// `approve` as the chief sends it: naming the request `result` shows pending in its session, if any.
pub(super) fn answer_pending(app: &mut Desktop<'_>, mut value: serde_json::Value) -> ControlResponse {
    if value.get("request").is_none() {
        let session = value["session"].clone();
        value["request"] = request(app, json!({"cmd":"result", "session":session})).result["approval"]["id"].clone();
    }
    request(app, value)
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

fn permission_hooks_wake_waiters_and_accept_decisions(app: &mut Desktop<'_>, _ctx: &egui::Context) {
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
            ControlRequest::Wait {
                compact: false,
                full_sessions: false,
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
        let id = app.runtime.sessions[0].approvals[0].id.clone();
        assert_eq!(
            response.result["events"][0]["approval"]["id"],
            json!(id),
            "the chief sees which request it answers"
        );
        let (tx, rx) = channel();
        app.runtime.control(
            ControlRequest::Approve {
                session: "codex".into(),
                request: Some(id),
                decision,
                message: Some("Reviewed".into()),
            },
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
    let expired = app.runtime.sessions[0].approvals[0].id.clone();
    let error = app.runtime.approve(0, &expired, Decision::Allow, None).unwrap_err();
    assert_eq!(error.message, "the permission hook has returned; answer in the agent terminal");
    assert_eq!(app.runtime.sessions[0].approvals.len(), 1, "the fallback menu still has time to draw");
    assert_eq!(app.runtime.resolution(&expired), None);
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

/// As in a smoke test: the chief decided about a request after it had been answered, and its decision,
/// which named only the session, answered the newer request it had never seen (a reset that wiped
/// databases). A decision now names its request and answers nothing else.
#[test]
fn a_late_decision_never_answers_a_newer_request() {
    with_app("app::tests::a_late_decision_never_answers_a_newer_request", |app, _| {
        let dir = crate::test_support::tempdir().unwrap();
        app.runtime.sessions = vec![session("codex", AgentKind::Generic, AgentStatus::Working, dir.path())];
        app.runtime.sessions[0].claude_id = Some("s1".into());
        let permission = |command: &str| {
            let reply = Arc::new(ipc::ApprovalReply::default());
            let event = atomics_core::hook::HookEvent::from_payload(&json!({
                "hook_event_name":"PermissionRequest", "session_id":"s1", "cwd":dir.path(),
                "tool_name":"Bash", "tool_input":{"command":command}
            }))
            .unwrap();
            let hook = HookMessage {
                pane: Some("codex".into()),
                ancestors: Vec::new(),
                event,
                delivered: 0,
                approval_reply: Some(reply.clone()),
            };
            (hook, reply)
        };
        let (hook, first) = permission("touch first-file");
        app.runtime.on_hook(hook);
        let seen = request(app, json!({"cmd":"list"})).result["sessions"][0]["approval"]["id"].clone();
        assert!(seen.is_string(), "list shows the request's id");
        // The user answers it on the desktop meanwhile, and the agent asks again.
        app.runtime.decide(seen.as_str().unwrap(), Decision::Allow, None).unwrap();
        assert!(first.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
        let (hook, second) = permission("git reset --hard");
        app.runtime.on_hook(hook);
        let current = app.runtime.sessions[0].approvals[0].id.clone();

        let late = request(
            app,
            json!({"cmd":"approve", "session":"codex", "request":seen, "decision":"allow", "message":"Max approved reset"}),
        );
        assert!(!late.ok);
        let error = late.error.unwrap();
        assert!(error.contains("no longer pending") && error.contains(&current), "{error}");
        assert!(error.contains("git reset --hard"), "the error shows the request now pending: {error}");
        assert_eq!(late.result["pending"]["id"], json!(current));
        let waiting =
            |app: &Desktop<'_>| app.runtime.sessions[0].approvals.iter().map(|p| p.id.clone()).collect::<Vec<_>>();
        assert_eq!(waiting(app), std::slice::from_ref(&current), "the newer request still waits");
        let unnamed = request(app, json!({"cmd":"approve", "session":"codex", "decision":"allow"}));
        assert!(!unnamed.ok && unnamed.error.unwrap().contains(&current));
        assert_eq!(waiting(app), std::slice::from_ref(&current), "a decision naming no request answers nothing");

        let answered = request(app, json!({"cmd":"approve", "session":"codex", "request":current, "decision":"deny"}));
        assert!(answered.ok, "{answered:?}");
        assert!(second.wait(Duration::ZERO).stdout.unwrap().contains("deny"));
    });
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
        app.runtime.theme.terminal_colors(),
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
        ControlRequest::PlanSet {
            ticket: None,
            plan: None,
            goal: "ship".into(),
            tasks: vec![plan::TaskInput { id: "a".into(), repo: Some("/worktree/src".into()), ..Default::default() }],
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
