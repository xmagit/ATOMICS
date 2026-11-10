//! The setup screen and the question a refused start asks, in the desktop's own paint order, on a home with no config
//! where it matters: a first run has to reach a first agent with clicks alone.
use super::sandbox_tests::tick_until;
use super::tests::{repository, request, session, with_app, with_login_state};
use super::*;
use atomics_core::config::AgentKind;
use atomics_core::isolation::IsolationMode;
use atomics_core::ops::{HostAction, SettingsTool};

type Texts = Vec<(String, Pos2)>;

fn frame(app: &mut Desktop<'_>, ctx: &egui::Context, events: Vec<egui::Event>) -> Texts {
    let output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1400.0, 1000.0))),
            time: Some(ctx.input(|input| input.time) + 0.1),
            events,
            ..Default::default()
        },
        |ui| app.paint(ui),
    );
    app.runtime.drain_actions();
    let visible: Texts = output
        .shapes
        .iter()
        .filter_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if shape.clip_rect.contains_rect(text.visual_bounding_rect()) => {
                Some((text.galley.text().to_string(), text.visual_bounding_rect().center()))
            }
            _ => None,
        })
        .collect();
    ctx.data_mut(|data| data.insert_temp(egui::Id::NULL.with("visible-setup-text"), visible));
    let texts = output
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
    texts
}

fn settle(app: &mut Desktop<'_>, ctx: &egui::Context) -> Texts {
    frame(app, ctx, vec![]);
    frame(app, ctx, vec![]);
    frame(app, ctx, vec![])
}

fn has(texts: &Texts, text: &str) -> bool {
    texts.iter().any(|(shown, _)| shown == text)
}

fn click_at(app: &mut Desktop<'_>, ctx: &egui::Context, pos: Pos2) -> Texts {
    let event = |pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    };
    frame(app, ctx, vec![egui::Event::PointerMoved(pos)]);
    frame(app, ctx, vec![event(true)]);
    frame(app, ctx, vec![event(false)]);
    settle(app, ctx)
}

fn click(app: &mut Desktop<'_>, ctx: &egui::Context, label: &str) -> Texts {
    let shown = settle(app, ctx);
    let pos = shown.iter().find(|(text, _)| text == label).unwrap_or_else(|| panic!("{label} in {shown:?}")).1;
    click_at(app, ctx, pos)
}

/// The text `label` on the row of `row`.
fn click_beside(app: &mut Desktop<'_>, ctx: &egui::Context, row: &str, label: &str) -> Texts {
    for _ in 0..24 {
        settle(app, ctx);
        let visible =
            ctx.data(|data| data.get_temp::<Texts>(egui::Id::NULL.with("visible-setup-text"))).unwrap_or_default();
        if let Some((_, at)) = visible.iter().find(|(text, _)| text == row)
            && let Some((_, pos)) = visible
                .iter()
                .filter(|(text, _)| text == label)
                .min_by(|a, b| (a.1.y - at.y).abs().total_cmp(&(b.1.y - at.y).abs()))
        {
            return click_at(app, ctx, *pos);
        }
        frame(
            app,
            ctx,
            vec![
                egui::Event::PointerMoved(pos2(700.0, 500.0)),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: vec2(0.0, -100.0),
                    source: egui::MouseWheelSource::Wheel,
                    phase: egui::TouchPhase::Move,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
    }
    panic!("{label} beside {row} is not visible: {:?}", settle(app, ctx));
}

/// The window's own control requests, as `App` hands them to the host, with the replies a window applies.
fn run_requests(app: &mut Desktop<'_>) {
    for command in std::mem::take(&mut app.runtime.client_commands) {
        let response = request(app, serde_json::to_value(&command).unwrap());
        assert!(response.ok, "{:?}", response.error);
        app.control_reply(true, response);
    }
}

/// A home with nothing in it, as a person's has, whose login shell's PATH finds these programs.
fn fresh_home(home: &Path, programs: &[&str]) -> Vec<(&'static str, std::ffi::OsString)> {
    let tools = home.join("login-tools");
    crate::test_support::link_tools(
        &tools,
        &["cat", "chmod", "mkdir", "sleep", "git", "env", "sh", "rm", "cp", "stty", "ps", "touch"],
    );
    for program in programs {
        crate::test_support::write_program(tools.join(program), "#!/bin/sh\nexec cat\n");
    }
    let login = tools.display().to_string();
    let shell = home.join("login-shell");
    crate::test_support::write_program(
        &shell,
        format!("#!/bin/sh\n[ \"$1 $2 $3\" = '-l -i -c' ] || exit 64\nPATH='{login}'\nexport PATH\neval \"$4\"\n"),
    );
    vec![
        (super::tests::FRESH_HOME, "1".into()),
        ("PATH", login.into()),
        ("XDG_CONFIG_HOME", home.join("xdg-config").into_os_string()),
        ("SHELL", shell.into_os_string()),
    ]
}

fn with_fresh_app(
    name: &str,
    programs: &'static [&'static str],
    test: impl FnOnce(&mut Desktop<'_>, &egui::Context, &Path),
) {
    with_login_state(
        name,
        |home| fresh_home(home, programs),
        |mut state, ctx| {
            let mut client = Client::new(ctx, &mut Options::default(), &crate::load_icon());
            let home = paths::home();
            test(&mut Desktop { runtime: &mut state, client: &mut client }, ctx, &home);
            state.shutdown();
        },
    );
}

fn look_for_programs(app: &mut Desktop<'_>, ctx: &egui::Context) {
    frame(app, ctx, vec![]);
    frame(app, ctx, vec![]);
    tick_until(app, ctx, "the programs", |app| {
        app.runtime.programs_check.is_none() && !app.runtime.setup_programs.is_empty()
    });
}

#[test]
fn a_fresh_home_reaches_a_first_agent_with_clicks_and_every_connection_can_wait() {
    with_fresh_app(
        "app::setup_tests::a_fresh_home_reaches_a_first_agent_with_clicks_and_every_connection_can_wait",
        &["claude"],
        |app, ctx, _| {
            assert!(app.runtime.store.setup_pending, "no config means a first run");
            let file = std::fs::read_to_string(paths::config_file()).unwrap();
            assert!(file.contains("\nmode = \"bare\"\n"), "a new install runs on this machine: {file}");

            look_for_programs(app, ctx);
            let shown = settle(app, ctx);
            assert!(app.client.ui.management.setup, "it opens by itself");
            for step in ["Your agents", "Where agents run", "A first project", "Connections", "Claude Code", "Codex"] {
                assert!(has(&shown, step), "{step}: {shown:?}");
            }
            assert!(!has(&shown, "Project"), "it has the window to itself: {shown:?}");
            assert!(has(&shown, "not found"), "Codex is not there: {shown:?}");
            assert!(has(&shown, "Start Claude Code") && !has(&shown, "Start Codex"), "{shown:?}");

            // Every connection waits with a click, nothing asks about it again, and that lasts: it is on the disk at once.
            for row in ["GitHub", "Azure DevOps", "Tickets", "Remote devices"] {
                let shown = click_beside(app, ctx, row, "Connect later");
                assert!(has(&shown, row), "{shown:?}");
            }
            let shown = settle(app, ctx);
            assert!(!has(&shown, "Connect later"), "{shown:?}");
            assert_eq!(Store::load(&paths::store_file()).setup_later, ["github", "azure", "tickets", "remote"]);
            // And it can be taken back.
            let shown = click_beside(app, ctx, "Tickets", "Connect now");
            assert!(has(&shown, "Connect later"), "{shown:?}");
            assert_eq!(Store::load(&paths::store_file()).setup_later, ["github", "azure", "remote"]);
            click_beside(app, ctx, "Tickets", "Connect later");

            // The first Start works, with nothing else done.
            click(app, ctx, "Start Claude Code");
            assert!(!app.client.ui.management.setup, "the agent takes the window");
            assert!(!app.runtime.store.setup_pending, "done with");
            assert!(!Store::load(&paths::store_file()).setup_pending, "and it stays done");
            run_requests(app);
            let [session] = &app.runtime.sessions[..] else { panic!("one agent: {:?}", app.runtime.sessions.len()) };
            assert!(session.sandbox.is_none(), "on this machine");
            assert!(session.scratch.is_some(), "in a scratch folder, with no project picked");
            let id = session.id.clone();
            assert!(app.runtime.start_choice.is_none());

            // Ctrl+1 reaches it from here, and nothing types into its terminal.
            let modifiers = if cfg!(target_os = "macos") { egui::Modifiers::MAC_CMD } else { egui::Modifiers::CTRL };
            app.runtime.diff_open = false;
            frame(
                app,
                ctx,
                vec![egui::Event::Key {
                    key: Key::Num1,
                    physical_key: Some(Key::Num1),
                    pressed: true,
                    repeat: false,
                    modifiers,
                }],
            );
            assert_eq!(app.runtime.active.as_deref(), Some(id.as_str()));
            assert!(app.runtime.diff_open && app.runtime.panel == PanelMode::Diff, "its diff shows");
        },
    );
}

#[test]
fn an_agent_installed_while_the_screen_shows_is_found_and_starts_without_a_restart() {
    with_fresh_app(
        "app::setup_tests::an_agent_installed_while_the_screen_shows_is_found_and_starts_without_a_restart",
        &[],
        |app, ctx, home| {
            look_for_programs(app, ctx);
            let shown = settle(app, ctx);
            assert!(has(&shown, "Claude Code") && !has(&shown, "Start Claude Code"), "{shown:?}");
            // A click where its installer's tools are here, and the command where they are not.
            assert!(
                has(&shown, "Install") || shown.iter().any(|(text, _)| text.starts_with("Copy: curl")),
                "how to get it: {shown:?}"
            );

            crate::test_support::write_program(home.join("login-tools/claude"), "#!/bin/sh\nexec cat\n");
            click_beside(app, ctx, "Your agents", "Check again");
            tick_until(app, ctx, "the second look", |app| app.runtime.programs_check.is_none());
            click(app, ctx, "Start Claude Code");
            run_requests(app);
            let session = &app.runtime.sessions[0];
            assert_ne!(session.status, AgentStatus::Exited, "{:?}", session.last_message);
            assert!(crate::setup::find_program("claude").is_some(), "what starts from now on finds it too");
        },
    );
}

/// As Claude Code's installer leaves it, with what a stand-in for `curl` prints for `bash` to run.
fn stand_in_installer(home: &Path, script: &str) {
    let tools = home.join("login-tools");
    crate::test_support::write_program(tools.join("curl"), format!("#!/bin/sh\ncat <<'SCRIPT'\n{script}\nSCRIPT\n"));
    crate::test_support::write_program(tools.join("bash"), "#!/bin/sh\nexec /bin/sh\n");
}

const INSTALLS_CLAUDE: &str = "mkdir -p \"$HOME/.local/bin\"\nprintf '#!/bin/sh\\nexec cat\\n' > \"$HOME/.local/bin/claude\"\nchmod +x \"$HOME/.local/bin/claude\"";

#[test]
fn claude_code_installs_with_a_click_and_starts_without_a_terminal_or_a_restart() {
    with_fresh_app(
        "app::setup_tests::claude_code_installs_with_a_click_and_starts_without_a_terminal_or_a_restart",
        &[],
        |app, ctx, home| {
            // Before it looks, so that this look offers the click; it goes in a folder the login shell does not list.
            stand_in_installer(home, INSTALLS_CLAUDE);
            look_for_programs(app, ctx);
            let shown = click_beside(app, ctx, "Claude Code", "Install");
            assert!(
                app.runtime.setup_job.is_some() || home.join(".local/bin/claude").exists(),
                "the click started it: {shown:?}"
            );
            tick_until(app, ctx, "the install", |app| app.runtime.setup_job.is_none());
            tick_until(app, ctx, "the look after it", |app| {
                !app.runtime.setup_look_again && app.runtime.programs_check.is_none()
            });
            assert!(app.runtime.setup_failure.is_none(), "{:?}", app.runtime.setup_failure);
            let shown = settle(app, ctx);
            assert!(has(&shown, "Start Claude Code"), "{shown:?}");
            click(app, ctx, "Start Claude Code");
            run_requests(app);
            let session = &app.runtime.sessions[0];
            assert_ne!(session.status, AgentStatus::Exited, "{:?}", session.last_message);
        },
    );
}

#[test]
fn a_step_shows_its_progress_can_be_stopped_and_a_failure_says_why_with_the_command_in_reach() {
    with_fresh_app(
        "app::setup_tests::a_step_shows_its_progress_can_be_stopped_and_a_failure_says_why_with_the_command_in_reach",
        &[],
        |app, ctx, home| {
            let marker = home.join("installer-started");
            stand_in_installer(home, &format!("touch '{}'\nexec sleep 30", marker.display()));
            look_for_programs(app, ctx);
            click_beside(app, ctx, "Claude Code", "Install");
            tick_until(app, ctx, "the installer to start", |_| marker.exists());
            let shown = settle(app, ctx);
            assert!(has(&shown, "Installing Claude Code…") && has(&shown, "Cancel"), "{shown:?}");
            let blockers = app.runtime.restart_blockers();
            assert!(blockers.iter().any(|blocker| blocker.contains("setup step")), "{blockers:?}");
            // One at a time.
            let another = HostAction::SettingsTool {
                tool: SettingsTool::RunSetup {
                    action: atomics_core::projection::SetupAction::InstallCodex,
                    value: None,
                },
            };
            assert!(app.runtime.manage(another).is_err());

            click(app, ctx, "Cancel");
            tick_until(app, ctx, "it to stop", |app| app.runtime.setup_job.is_none());
            assert!(app.runtime.setup_failure.is_none(), "stopping it is no failure");

            stand_in_installer(home, "echo 'no space left on device' >&2\nexit 7");
            tick_until(app, ctx, "the look after it", |app| {
                !app.runtime.setup_look_again && app.runtime.programs_check.is_none()
            });
            let shown = click_beside(app, ctx, "Claude Code", "Install");
            tick_until(app, ctx, "the failure", |app| app.runtime.setup_job.is_none());
            let failure = app.runtime.setup_failure.clone().expect("it failed");
            assert!(failure.why.contains("no space left on device"), "{failure:?} {shown:?}");
            let shown = settle(app, ctx);
            assert!(shown.iter().any(|(text, _)| text.contains("no space left")), "{shown:?}");
            assert!(
                shown.iter().any(|(text, _)| text.starts_with("Copy: curl")),
                "what it ran stays in reach: {shown:?}"
            );

            // A download that fails says nothing to `bash`, and is no success.
            stand_in_installer(home, "exit 0");
            click_beside(app, ctx, "Claude Code", "Install");
            tick_until(app, ctx, "the second failure", |app| app.runtime.setup_job.is_none());
            let failure = app.runtime.setup_failure.clone().expect("nothing was installed");
            assert!(failure.why.contains("not on your PATH"), "{failure:?}");
        },
    );
}

#[test]
fn settings_runs_every_sandbox_step_with_clicks_and_reads_back_what_is_left() {
    with_app(
        "app::setup_tests::settings_runs_every_sandbox_step_with_clicks_and_reads_back_what_is_left",
        |app, ctx| {
            let bin = crate::test_support::tempdir().unwrap();
            app.runtime.sbx = crate::sandbox::fake::install(bin.path()).display().to_string();
            for marker in ["signed-out", "no-policy", "removed-anthropic", "no-openai"] {
                std::fs::write(bin.path().join(marker), "").unwrap();
            }
            let wait = |app: &mut Desktop<'_>| {
                frame(app, ctx, vec![]);
                tick_until(app, ctx, "the step and the look after it", |app| {
                    app.runtime.setup_job.is_none()
                        && !app.runtime.setup_look_again
                        && app.runtime.sandbox_check.is_none()
                        && app.runtime.programs_check.is_none()
                });
                settle(app, ctx)
            };
            app.client.ui.management.settings = true;
            settle(app, ctx);
            wait(app);
            let shown = settle(app, ctx);
            assert!(has(&shown, "Docker account") && has(&shown, "Network policy"), "{shown:?}");
            assert!(has(&shown, "What they need"), "{shown:?}");
            assert!(has(&shown, "Docker Sandboxes"), "{shown:?}");

            click_beside(app, ctx, "Docker account", "Sign in");
            let shown = wait(app);
            assert!(!shown.iter().any(|(text, _)| text == "Sign in"), "signed in: {shown:?}");
            assert_eq!(crate::sandbox::fake::calls_to(bin.path(), &["login"]).len(), 1);

            click_beside(app, ctx, "Network policy", "Use balanced");
            let shown = wait(app);
            assert!(!has(&shown, "Use balanced"), "{shown:?}");
            assert_eq!(crate::sandbox::fake::calls_to(bin.path(), &["policy", "init"]).len(), 1);

            // A pasted value leaves the field as it is sent, reaches no argument, and shows nowhere.
            const FIELD: &str = "Anthropic API key or token";
            let paste = |app: &mut Desktop<'_>, value: &str| {
                click_beside(app, ctx, "Claude in sandboxes", FIELD);
                frame(app, ctx, vec![egui::Event::Text(value.into())]);
            };
            paste(app, "sk-ant-api03-PRIVATE-not-sent");
            let shown = settle(app, ctx);
            assert!(!has(&shown, FIELD), "the field holds it, where the hint was: {shown:?}");
            assert!(!format!("{shown:?}").contains("PRIVATE"), "and it is masked: {shown:?}");
            // What was not sent goes when Settings closes, and comes back to nobody.
            app.client.ui.management.settings = false;
            settle(app, ctx);
            app.client.ui.management.settings = true;
            let shown = settle(app, ctx);
            assert!(has(&shown, FIELD), "{shown:?}");

            paste(app, "sk-ant-api03-PRIVATE-pasted");
            click_beside(app, ctx, "Claude in sandboxes", "Save");
            let shown = wait(app);
            assert!(!format!("{shown:?}").contains("PRIVATE"), "{shown:?}");
            assert!(!format!("{:?}", crate::sandbox::fake::calls(bin.path())).contains("PRIVATE"));
            assert!(!format!("{:?}", app.runtime.management_info().setup).contains("PRIVATE"));
            assert_eq!(
                std::fs::read_to_string(bin.path().join("secret-stdin")).unwrap().trim(),
                "sk-ant-api03-PRIVATE-pasted"
            );
            assert!(!has(&shown, FIELD), "stored, so there is no field to fill: {shown:?}");
        },
    );
}

#[test]
fn the_welcome_starts_agents_in_a_project_and_never_in_a_folder_that_is_not_one() {
    with_app(
        "app::setup_tests::the_welcome_starts_agents_in_a_project_and_never_in_a_folder_that_is_not_one",
        |app, ctx| {
            assert!(!app.runtime.launch_project, "this test's folder is no repository");
            assert_eq!(app.runtime.backend().launch_dir(), None);
            let shown = settle(app, ctx);
            assert!(has(&shown, "Start claude"), "{shown:?}");
            assert!(shown.iter().any(|(text, _)| text.starts_with("each in a scratch folder")), "{shown:?}");
            app.runtime.launch_project = true;
            let launch = app.runtime.launch_dir.clone();
            assert_eq!(app.runtime.backend().launch_dir(), Some(launch.clone()));
            let shown = settle(app, ctx);
            assert!(shown.iter().any(|(text, _)| text.starts_with("in ")), "{shown:?}");
        },
    );
}

#[test]
fn an_existing_install_gets_no_screen_and_keeps_its_settings() {
    with_app("app::setup_tests::an_existing_install_gets_no_screen_and_keeps_its_settings", |app, ctx| {
        let before = std::fs::read_to_string(paths::config_file()).unwrap();
        let shown = settle(app, ctx);
        assert!(!app.runtime.store.setup_pending && !app.client.ui.management.setup);
        assert!(!has(&shown, "Your agents"), "{shown:?}");
        assert_eq!(std::fs::read_to_string(paths::config_file()).unwrap(), before);
    });
}

#[test]
fn skipping_lasts_and_settings_brings_the_screen_back_as_a_window_over_agents() {
    with_app(
        "app::setup_tests::skipping_lasts_and_settings_brings_the_screen_back_as_a_window_over_agents",
        |app, ctx| {
            app.runtime.store.setup_pending = true;
            look_for_programs(app, ctx);
            assert!(app.client.ui.management.setup);
            frame(
                app,
                ctx,
                vec![egui::Event::Key {
                    key: Key::Escape,
                    physical_key: Some(Key::Escape),
                    pressed: true,
                    repeat: false,
                    modifiers: Default::default(),
                }],
            );
            let shown = settle(app, ctx);
            assert!(!app.client.ui.management.setup && !app.runtime.store.setup_pending, "Escape skips it");
            assert!(!has(&shown, "Your agents"), "{shown:?}");

            app.client.ui.management.settings = true;
            settle(app, ctx);
            click(app, ctx, "Getting started");
            let shown = click(app, ctx, "Open the setup screen");
            assert!(app.client.ui.management.setup && !app.client.ui.management.settings);
            assert!(has(&shown, "Your agents") && has(&shown, "Close"), "{shown:?}");
            assert!(!has(&shown, "Set up ATOMICS"), "with nothing running it has the window: {shown:?}");
            let shown = click(app, ctx, "Close");
            assert!(!app.client.ui.management.setup, "{shown:?}");

            // Over a running agent it is a window, and the agent stays on screen behind it.
            let root = paths::home();
            app.runtime.sessions = vec![session("agent", AgentKind::Generic, AgentStatus::Working, &root)];
            app.runtime.active = Some("agent".into());
            app.runtime.ungrouped = true;
            app.client.ui.management.setup = true;
            let shown = settle(app, ctx);
            assert!(has(&shown, "Set up ATOMICS") && has(&shown, "Your agents"), "{shown:?}");
            app.client.ui.management.setup = false;
            app.runtime.sessions.clear();
        },
    );
}

/// What a saved first run comes back to after any restart or update: no screen once it was skipped, and what was left for
/// later still left for later when Settings brings it back. Only a home with no config is a first run.
#[test]
fn what_was_skipped_and_left_for_later_stays_that_way_after_a_restart() {
    with_app("app::setup_tests::what_was_skipped_and_left_for_later_stays_that_way_after_a_restart", |app, ctx| {
        app.runtime.store.setup_later = vec!["github".into()];
        app.runtime.store.save(&paths::store_file()).unwrap();
        let saved = Store::load(&paths::store_file());
        assert_eq!(saved.setup_later, ["github"]);
        assert!(!saved.setup_pending);
        let shown = settle(app, ctx);
        assert!(!app.client.ui.management.setup && !has(&shown, "Your agents"), "{shown:?}");
        app.client.ui.management.setup = true;
        let shown = settle(app, ctx);
        assert!(has(&shown, "Later") && has(&shown, "Connect now"), "{shown:?}");
    });
}

#[test]
fn a_program_outside_where_it_belongs_offers_to_install_itself() {
    with_app("app::setup_tests::a_program_outside_where_it_belongs_offers_to_install_itself", |app, ctx| {
        assert!(app.runtime.install_offer.is_none(), "a test build stays where it is");
        app.client.ui.management.setup = true;
        assert!(!has(&settle(app, ctx), "Install ATOMICS"));

        let (from, to) = (PathBuf::from("/tmp/Downloads/atomics"), paths::home().join(".local/bin/atomics"));
        app.runtime.install_offer = Some((from, to));
        let shown = settle(app, ctx);
        assert!(has(&shown, "Install ATOMICS"), "{shown:?}");
        assert!(shown.iter().any(|(text, _)| text.contains("/tmp/Downloads/atomics")), "{shown:?}");
        let setup = app.runtime.setup_info().install.unwrap();
        assert_eq!(setup.from, "/tmp/Downloads/atomics");
        // With nothing running it reopens from there; over agents, the next start does.
        app.runtime.sessions = vec![session("agent", AgentKind::Generic, AgentStatus::Working, &paths::home())];
        app.runtime.installed_self_for_test();
        assert!(app.runtime.install_offer.is_none());
        assert!(!app.runtime.restarting, "it does not close over a running agent");
        app.runtime.sessions.clear();
        app.client.ui.management.setup = false;
    });
}

#[test]
fn a_person_whose_start_has_no_sandbox_gets_asked_and_an_agents_start_does_not() {
    with_app(
        "app::setup_tests::a_person_whose_start_has_no_sandbox_gets_asked_and_an_agents_start_does_not",
        |app, ctx| {
            let main = std::fs::read_to_string(paths::config_file()).unwrap();
            app.runtime
                .manage(HostAction::SettingsTool { tool: SettingsTool::SetIsolation { mode: IsolationMode::Sandbox } })
                .unwrap();
            assert_eq!(app.runtime.config.isolation.mode, IsolationMode::Sandbox);
            assert_eq!(
                std::fs::read_to_string(paths::config_file()).unwrap(),
                main,
                "this machine's own file takes it"
            );
            let ask = |app: &mut Desktop<'_>| {
                app.send(Command::Start { agent: "shell".into(), folder: None });
                run_requests(app);
                tick_until(app, ctx, "the sandbox's preparation", |app| app.runtime.pending_sandbox.is_empty());
            };

            // Leaving it takes the scratch folder with it, and nothing stays behind on screen.
            ask(app);
            let waiting = app.runtime.start_choice.as_ref().expect("asks");
            let scratch = waiting.cwd.clone();
            assert!(scratch.is_dir());
            assert!(app.runtime.sessions.is_empty(), "no failed tile");
            let why = app.runtime.management_info().setup.start_choice.unwrap().why;
            assert!(why.contains("not installed"), "{why}");
            assert!(!why.contains("[isolation]"), "the button is the way out, not a file to edit: {why}");
            let shown = settle(app, ctx);
            for label in ["Run on this machine", "Set up sandboxes", "Cancel"] {
                assert!(has(&shown, label), "{label}: {shown:?}");
            }
            click(app, ctx, "Cancel");
            assert!(app.runtime.start_choice.is_none() && !scratch.exists());
            assert_eq!(app.runtime.config.isolation.mode, IsolationMode::Sandbox, "nothing changed");

            // Setting them up goes to where that is done.
            ask(app);
            let shown = click(app, ctx, "Set up sandboxes");
            assert!(app.runtime.start_choice.is_none() && app.client.ui.management.settings);
            assert!(has(&shown, "Docker Sandboxes"), "the Agents section is open: {shown:?}");
            app.client.ui.management.settings = false;
            settle(app, ctx);

            // An agent's start has nobody to ask: it fails as before.
            let response = request(app, serde_json::json!({"cmd": "start", "agent": "shell"}));
            assert!(response.ok, "{:?}", response.error);
            tick_until(app, ctx, "the sandbox's preparation", |app| app.runtime.pending_sandbox.is_empty());
            assert!(app.runtime.start_choice.is_none());
            assert_eq!(app.runtime.sessions[0].status, AgentStatus::Exited);
            app.runtime.sessions.clear();

            // Running here runs it, and this machine keeps doing that.
            ask(app);
            let shown = click(app, ctx, "Run on this machine");
            assert!(app.runtime.start_choice.is_none(), "{shown:?}");
            let [session] = &app.runtime.sessions[..] else { panic!("the agent runs") };
            assert!(session.sandbox.is_none());
            assert_ne!(session.status, AgentStatus::Exited, "{:?}", session.last_message);
            assert_eq!(app.runtime.active.as_deref(), Some(session.id.as_str()));
            assert_eq!(app.runtime.config.isolation.mode, IsolationMode::Bare);
            let [_, host_file] = Config::source_paths(&app.runtime.config_path);
            assert!(std::fs::read_to_string(host_file).unwrap().contains("mode = \"bare\""));
        },
    );
}

#[test]
fn a_repository_that_requires_sandboxes_offers_only_setting_them_up() {
    with_app("app::setup_tests::a_repository_that_requires_sandboxes_offers_only_setting_them_up", |app, ctx| {
        let (_dir, repo) = repository();
        std::fs::create_dir_all(repo.join(".atomics")).unwrap();
        std::fs::write(repo.join(".atomics/config.toml"), "[isolation]\nmode = \"sandbox\"\n").unwrap();
        app.send(Command::Start { agent: "shell".into(), folder: Some(repo.clone()) });
        run_requests(app);
        tick_until(app, ctx, "the sandbox's preparation", |app| app.runtime.pending_sandbox.is_empty());
        let choice = app.runtime.management_info().setup.start_choice.expect("asks");
        assert!(choice.sandbox_required);
        let shown = settle(app, ctx);
        assert!(has(&shown, "Set up sandboxes") && !has(&shown, "Run on this machine"), "{shown:?}");
        // Nor can another client force it.
        app.runtime.manage(HostAction::SettingsTool { tool: SettingsTool::AnswerStart { run_here: true } }).unwrap();
        assert!(app.runtime.sessions.is_empty() && app.runtime.start_choice.is_none());
        assert!(repo.is_dir(), "only a scratch folder goes with the answer");
    });
}

#[test]
fn a_folder_with_no_repository_can_be_made_a_first_project_and_a_repository_stays_as_it_is() {
    with_app(
        "app::setup_tests::a_folder_with_no_repository_can_be_made_a_first_project_and_a_repository_stays_as_it_is",
        |app, _| {
            let dir = crate::test_support::tempdir().unwrap();
            let folder = dir.path().join("plain");
            std::fs::create_dir(&folder).unwrap();
            let init = |path: &Path| HostAction::SettingsTool {
                tool: SettingsTool::InitProject { path: path.to_string_lossy().into_owned() },
            };
            app.runtime.manage(init(&folder)).unwrap();
            assert!(folder.join(".git").exists());
            let root = folder.canonicalize().unwrap();
            assert!(app.runtime.open_projects.iter().any(|project| project.canonicalize().unwrap() == root));
            assert_eq!(app.runtime.setup_info().project, Some(paths::display(&app.runtime.open_projects[0])));

            let (_dir, repo) = repository();
            let head = std::fs::read_to_string(repo.join(".git/HEAD")).unwrap();
            app.runtime.manage(init(&repo)).unwrap();
            assert_eq!(std::fs::read_to_string(repo.join(".git/HEAD")).unwrap(), head);
            assert!(app.runtime.manage(init(&dir.path().join("missing"))).is_err());
        },
    );
}
