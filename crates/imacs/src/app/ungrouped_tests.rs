//! Sessions outside any plan: the Ungrouped tab, scratch folders and their names.
use super::input::new_session_request;
use super::tests::{repository, request, session, with_app};
use super::*;
use crate::sandbox::fake;
use imacs_core::isolation::IsolationMode;

/// A stand-in agent that writes the folder it runs in to `out`, then waits.
fn probe(name: &str, kind: AgentKind, out: &Path) -> imacs_core::config::AgentConfig {
    imacs_core::config::AgentConfig {
        name: name.into(),
        command: vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("pwd -P > {0}.$$.tmp; mv {0}.$$.tmp {0}; exec sleep 600", out.display()),
        ],
        kind,
        busy_text: None,
        isolation: None,
    }
}

fn tick_until(app: &mut Desktop<'_>, ctx: &egui::Context, what: &str, mut done: impl FnMut(&mut Desktop<'_>) -> bool) {
    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(60));
    while !done(app) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        app.runtime.runtime_tick(ctx);
        if !done(app) {
            app.runtime.wake.wait_until(deadline.min(Instant::now() + Duration::from_millis(50)));
        }
    }
}

fn frame(app: &mut Desktop<'_>, ctx: &egui::Context, events: Vec<egui::Event>) {
    let output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1400.0, 900.0))),
            events,
            ..Default::default()
        },
        |ui| app.dialogs(&ui.ctx().clone()),
    );
    output.drop_without_applying_deltas();
}

fn key(key: Key) -> egui::Event {
    egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }
}

/// Sends a start request and returns the new session's index.
fn start(app: &mut Desktop<'_>, value: serde_json::Value) -> usize {
    let response = request(app, value);
    assert!(response.ok, "{:?}", response.error);
    app.runtime.find(response.result["id"].as_str().unwrap()).unwrap()
}

#[test]
fn new_sessions_start_ungrouped_in_scratch_folders_named_by_their_first_prompt() {
    with_app(
        "app::ungrouped_tests::new_sessions_start_ungrouped_in_scratch_folders_named_by_their_first_prompt",
        |app, ctx| {
            let (dir, repo) = repository();
            let worktree = repo.parent().unwrap().join("repo-task");
            git::add_worktree(&repo, &worktree, "issue/7-checkout", "main").unwrap();
            let out = dir.path().join("pwd");
            app.runtime.config.agents.push(probe("probe", AgentKind::Generic, &out));
            app.runtime.open_project(&repo);
            // A plan task's tile has focus, in its plan's tab.
            let mut task = session("task", AgentKind::Claude, AgentStatus::Idle, &worktree);
            task.title = "7-checkout".into();
            app.runtime.sessions = vec![task];
            app.runtime.plans = vec![Plan {
                id: "p1".into(),
                project: Some(repo.clone()),
                tasks: vec![Task { id: "t1".into(), session: Some("task".into()), ..Task::default() }],
                ..Plan::default()
            }];
            app.runtime.selected_plan = 0;
            app.runtime.show_plan_fleet();
            app.runtime.activate("task", true);
            assert_eq!(app.runtime.diff_target().as_deref(), Some(worktree.as_path()));

            // The shortcut's dialog starts in a scratch folder and takes nothing from the focused tile.
            app.open_new_dialog(None);
            let dialog = app.client.new_dialog.as_mut().unwrap();
            assert_eq!(dialog.place, Place::Scratch);
            assert!(dialog.folder.is_empty(), "the focused worktree is not offered: {}", dialog.folder);
            dialog.agent = "probe".into();
            frame(app, ctx, vec![]);
            frame(app, ctx, vec![egui::Event::Text("Compare Rust HTTP clients for a CLI tool".into())]);
            frame(app, ctx, vec![key(Key::Enter)]);
            assert!(app.client.new_dialog.is_none());
            assert!(app.runtime.ungrouped && !app.runtime.plan_view, "the new session lands in Ungrouped");
            let commands = std::mem::take(&mut app.runtime.client_commands);
            let [request_sent] = &commands[..] else { panic!("one start request: {commands:?}") };
            let value = serde_json::to_value(request_sent).unwrap();
            assert_eq!(value["folder"], json!(null));
            assert_eq!(value["repo"], json!(null));
            let index = start(app, value);

            let scratch = app.runtime.sessions[index].scratch.clone().unwrap();
            assert_eq!(scratch.parent(), Some(paths::scratch_dir().as_path()));
            assert!(scratch.is_dir() && scratch != worktree);
            assert_eq!(app.runtime.sessions[index].cwd, scratch);
            assert_eq!(app.runtime.sessions[index].title, "Compare Rust HTTP clients for a CLI tool");
            tick_until(app, ctx, "the probe to start", |_| out.exists());
            assert_eq!(
                std::fs::read_to_string(&out).unwrap().trim(),
                scratch.canonicalize().unwrap().display().to_string()
            );
            tick_until(app, ctx, "its folder check", |app| app.runtime.sessions[index].worktree_ready.is_none());
            assert_eq!(app.runtime.sessions[index].title, "Compare Rust HTTP clients for a CLI tool");

            // The Ungrouped tab shows it and not the plan's task, whose tile keeps its own name.
            let id = app.runtime.sessions[index].id.clone();
            assert_eq!(app.runtime.active.as_deref(), Some(id.as_str()));
            assert_eq!(app.runtime.visible_sessions(), [index]);
            assert_eq!(app.runtime.sessions[0].title, "7-checkout");
            // Its own repository, without commits, shows what the agent writes there as new.
            assert_eq!(app.runtime.diff_target().unwrap().canonicalize().unwrap(), scratch.canonicalize().unwrap());
            std::fs::write(scratch.join("notes.md"), "# HTTP clients\n").unwrap();
            let diff = git::diff_against_base(&scratch, "origin/main", None).unwrap();
            assert_eq!(diff.base_ref, "(no commits)");
            assert_eq!(diff.files.iter().map(|file| file.path()).collect::<Vec<_>>(), ["notes.md"]);
            assert!(app.runtime.completed_tasks().is_empty());
            app.runtime.show_plan_fleet();
            assert_eq!(app.runtime.visible_sessions(), [0]);
            assert!(request(app, json!({"cmd":"activate","session":id})).ok);
            assert!(app.runtime.ungrouped, "focusing it brings up the Ungrouped tab");
            assert_eq!(app.runtime.visible_sessions(), [index]);
            app.runtime.set_view(None);
            assert!(!app.runtime.ungrouped && app.runtime.visible_sessions().len() == 2);

            // Renamed, it keeps the name; later prompts never rename it.
            app.begin_rename(&id);
            app.client.rename.as_mut().unwrap().title = "  HTTP clients  ".into();
            frame(app, ctx, vec![key(Key::Enter)]);
            app.runtime.drain_actions();
            assert!(app.client.rename.is_none());
            assert_eq!(app.runtime.sessions[index].title, "HTTP clients");
            app.runtime.deliver(index, "Now benchmark them");
            assert_eq!(app.runtime.sessions[index].title, "HTTP clients");

            // A session without hooks is named after the first line typed into its terminal.
            let typed = start(app, json!({"cmd":"start","agent":"probe"}));
            assert_eq!(app.runtime.sessions[typed].title, UNTITLED);
            let terminal = app.runtime.sessions[typed].terminal.as_ref().unwrap();
            *terminal.shared.submitted.lock().unwrap() = Some("/login".into());
            app.runtime.runtime_tick(ctx);
            assert_eq!(app.runtime.sessions[typed].title, UNTITLED, "a slash command names nothing");
            let terminal = app.runtime.sessions[typed].terminal.as_ref().unwrap();
            *terminal.shared.submitted.lock().unwrap() = Some("Draft the release notes".into());
            app.runtime.runtime_tick(ctx);
            assert_eq!(app.runtime.sessions[typed].title, "Draft the release notes");
            // Each scratch session has a folder of its own.
            assert_ne!(app.runtime.sessions[typed].scratch, Some(scratch.clone()));

            // A folder the user picks is used, and that session is named after it.
            app.open_new_dialog(None);
            let dialog = app.client.new_dialog.as_mut().unwrap();
            dialog.place = Place::Folder;
            assert!(new_session_request(dialog).is_err(), "an empty folder is refused");
            dialog.folder = paths::display(&worktree);
            let ControlRequest::Start { folder, .. } = new_session_request(dialog).unwrap() else { unreachable!() };
            assert_eq!(folder.map(PathBuf::from), Some(worktree.clone()));
            app.client.new_dialog = None;

            for id in app.runtime.sessions.iter().map(|s| s.id.clone()).collect::<Vec<_>>() {
                app.runtime.close(&id);
            }
            app.runtime.finish_lifecycle_jobs();
            assert!(!scratch.exists(), "closing removes its scratch folder");
            assert!(worktree.is_dir(), "a picked folder stays");
            assert_eq!(std::fs::read_dir(paths::scratch_dir()).unwrap().count(), 0);
            assert!(
                !app.runtime.store.sessions.iter().any(|record| paths::scratch_of(&record.cwd).is_some()),
                "scratch sessions are never offered for resuming"
            );
        },
    );
}

#[test]
fn scratch_sessions_follow_the_isolation_setting_and_go_with_their_sandbox() {
    with_app(
        "app::ungrouped_tests::scratch_sessions_follow_the_isolation_setting_and_go_with_their_sandbox",
        |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            let bin = crate::test_support::tempdir().unwrap();
            app.runtime.sbx = fake::install(bin.path()).display().to_string();
            std::fs::write(bin.path().join("run-waits"), "wait").unwrap();
            app.runtime.config.isolation.mode = IsolationMode::Sandbox;
            let out = dir.path().join("pwd");
            app.runtime.config.agents.push(probe("probe", AgentKind::Shell, &out));

            // Sandboxed by default: the sandbox mounts the scratch folder.
            let boxed = start(app, json!({"cmd":"start","agent":"probe","prompt":"Read the RFC"}));
            let id = app.runtime.sessions[boxed].id.clone();
            tick_until(app, ctx, "the sandbox to attach", |app| {
                app.runtime.sessions.iter().any(|s| s.id == id && s.terminal.is_some())
            });
            let boxed = app.runtime.find(&id).unwrap();
            let scratch = app.runtime.sessions[boxed].scratch.clone().unwrap();
            let name = format!("imacs-{id}");
            assert_eq!(app.runtime.sessions[boxed].sandbox.as_deref(), Some(name.as_str()));
            assert_eq!(app.runtime.sessions[boxed].title, "Read the RFC");
            let create = &fake::calls_to(bin.path(), &["create"])[0];
            let agent = create.iter().position(|arg| arg == "shell").unwrap();
            assert_eq!(Path::new(&create[agent + 1]), scratch.canonicalize().unwrap(), "it mounts the scratch folder");

            // Bare for this one session only.
            let bare = start(app, json!({"cmd":"start","agent":"probe","isolation":"bare"}));
            let bare_id = app.runtime.sessions[bare].id.clone();
            let bare_scratch = app.runtime.sessions[bare].scratch.clone().unwrap();
            assert!(app.runtime.sessions[bare].sandbox.is_none() && app.runtime.sessions[bare].terminal.is_some());
            tick_until(app, ctx, "the bare probe", |_| out.exists());
            assert_eq!(
                std::fs::read_to_string(&out).unwrap().trim(),
                bare_scratch.canonicalize().unwrap().display().to_string()
            );
            assert_eq!(fake::calls_to(bin.path(), &["create"]).len(), 1);

            app.runtime.close(&bare_id);
            app.runtime.finish_lifecycle_jobs();
            assert!(!bare_scratch.exists() && scratch.is_dir());
            app.runtime.close(&id);
            app.runtime.finish_lifecycle_jobs();
            assert!(!scratch.exists());
            assert_eq!(fake::calls_to(bin.path(), &["stop"]), [["stop", name.as_str()]]);
            assert_eq!(fake::calls_to(bin.path(), &["rm"]), [["rm", "--force", name.as_str()]], "it is never resumed");
            assert!(!bin.path().join("sandboxes").join(&name).exists());

            // Closed while its sandbox is made: the folder goes at once, the sandbox once it is ready.
            std::fs::write(bin.path().join("wait-create"), "wait").unwrap();
            let late = start(app, json!({"cmd":"start","agent":"probe"}));
            let late_id = app.runtime.sessions[late].id.clone();
            let late_scratch = app.runtime.sessions[late].scratch.clone().unwrap();
            tick_until(app, ctx, "sandbox creation to begin", |_| bin.path().join("waiting-create").exists());
            app.runtime.close(&late_id);
            app.runtime.finish_lifecycle_jobs();
            assert!(!late_scratch.exists());
            std::fs::remove_file(bin.path().join("wait-create")).unwrap();
            tick_until(app, ctx, "the late sandbox", |app| app.runtime.pending_sandbox.is_empty());
            app.runtime.finish_lifecycle_jobs();
            let late_name = format!("imacs-{late_id}");
            let removed = fake::calls_to(bin.path(), &["rm"]);
            assert!(removed.iter().any(|call| call == &["rm", "--force", late_name.as_str()]), "{removed:?}");
        },
    );
}

#[test]
fn quitting_ends_scratch_sessions_and_startup_sweeps_what_a_crash_left() {
    with_app("app::ungrouped_tests::quitting_ends_scratch_sessions_and_startup_sweeps_what_a_crash_left", |app, _| {
        let (dir, repo) = repository();
        app.runtime.config.agents.push(probe("probe", AgentKind::Generic, &dir.path().join("pwd")));
        let started = start(app, json!({"cmd":"start","agent":"probe"}));
        let scratch = app.runtime.sessions[started].scratch.clone().unwrap();
        start(app, json!({"cmd":"start","agent":"probe","folder":repo}));

        // What a crashed instance left, and a folder a restart's handoff resumes, both from before startup.
        let home = paths::imacs_home();
        let (crashed, resumed) = (paths::scratch_dir().join("crashed1"), paths::scratch_dir().join("resumed1"));
        let hour_ago = SystemTime::now() - Duration::from_secs(3600);
        for folder in [&crashed, &resumed] {
            std::fs::create_dir_all(folder.join("notes")).unwrap();
            std::fs::File::open(folder).unwrap().set_modified(hour_ago).unwrap();
        }
        let started_at = SystemTime::now() - Duration::from_secs(60);
        let notes = crate::lifecycle::sweep_scratch(&home, &HashSet::from([resumed.clone()]), started_at);
        assert!(notes.is_empty(), "{notes:?}");
        assert!(!crashed.exists(), "an old scratch folder no session resumes goes");
        assert!(resumed.is_dir(), "the handoff's folder stays");
        assert!(scratch.is_dir(), "a folder made after startup stays");

        app.runtime.shutdown();
        assert!(!scratch.exists(), "quitting removes scratch folders");
        assert!(repo.is_dir() && resumed.is_dir());
    });
}

#[test]
fn removing_scratch_folders_never_reaches_outside_scratch() {
    let dir = crate::test_support::tempdir().unwrap();
    let home = dir.path().join("state");
    let precious = dir.path().join("precious");
    std::fs::create_dir_all(home.join("scratch/a1")).unwrap();
    std::fs::create_dir(&precious).unwrap();
    for outside in
        [precious.clone(), home.join("scratch"), home.join("scratch/a1/inner"), home.join("scratch/../precious")]
    {
        assert!(crate::lifecycle::remove_scratch(&home, &outside).is_err(), "{}", outside.display());
    }
    assert!(precious.is_dir() && home.join("scratch/a1").is_dir());
    crate::lifecycle::remove_scratch(&home, &home.join("scratch/a1")).unwrap();
    crate::lifecycle::remove_scratch(&home, &home.join("scratch/a1")).unwrap();
    assert!(!home.join("scratch/a1").exists());
}
