//! Phase 3's gates for the native client (`atomics --connect`, `client.rs`) against a real host: the
//! runtime with its protocol server, as `atomics host` runs it, and the client's own driver, link and
//! backend. Closing the window and opening it again keeps the agents' processes, their terminals and the
//! plans; two windows share one board and one lease; a window that stalls catches up.

use std::time::{Duration, Instant};

use atomics_core::control::{ControlRequest, ControlResponse};
use serde_json::{Value, json};

use super::tests::with_state;
use crate::client::target::Target;
use crate::client::{ClientApp, Launch};

fn ctl(request: Value) -> Value {
    let request: ControlRequest = serde_json::from_value(request).unwrap();
    let response: ControlResponse = crate::ipc::request(&request).unwrap();
    assert!(response.ok, "{:?}", response.error);
    response.result
}

/// Pump `app` (and follow `screen`, as a drawn terminal does) until `done`.
fn until(app: &mut ClientApp, screen: Option<&str>, what: &str, done: impl Fn(&ClientApp) -> bool) {
    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(20));
    loop {
        if let Some(session) = screen {
            let remote = &app.driver().remote;
            remote.show_screen(session, true);
            remote.end_frame();
            app.driver_mut().flush();
        }
        app.pump();
        if done(app) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {:?} {:?}",
            app.driver().status(),
            screen.map(|session| screen_text(app, session))
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn screen_text(app: &ClientApp, session: &str) -> String {
    app.driver().remote.with_screen(session, |screen| screen.map(|(s, _)| s.text()).unwrap_or_default())
}

fn has_session(app: &ClientApp, session: &str) -> bool {
    app.driver().remote.with_mirror(|m| m.sessions.contains_key(session))
}

fn pid_of(session: &str) -> u64 {
    ctl(json!({"cmd":"list"}))["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == session)
        .and_then(|s| s["pid"].as_u64())
        .expect("the session's process")
}

fn alive(pid: u64) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// A controller may type into a guest terminal. The fixture acknowledges lines without executing them.
fn configure_guest_terminal(state: &mut super::State) {
    let bundle = crate::vm::installed_dir();
    let program = bundle.join("bin/guest-terminal");
    std::fs::create_dir_all(program.parent().unwrap()).unwrap();
    crate::test_support::write_program(
        &program,
        "#!/bin/sh\necho 'terminal ready'\nwhile IFS= read -r line; do printf 'received: %s\\n' \"$line\"; done\n",
    );
    let mut agent = state.config.agent("shell").unwrap().clone();
    agent.name = "guest-terminal".into();
    agent.command = vec![program.display().to_string()];
    state.config.agents.push(agent);
    std::fs::write(
        bundle.join("config.d.toml"),
        format!("[[agents]]\nname='guest-terminal'\ncommand=['{}']\n", program.display()),
    )
    .unwrap();
    state.config.remote.enabled = true;
    super::tests::persist_config(state);
}

#[test]
fn closing_the_window_leaves_the_agents_and_opening_it_again_shows_them_as_they_were() {
    with_state(
        "app::client_tests::closing_the_window_leaves_the_agents_and_opening_it_again_shows_them_as_they_were",
        |mut state, ctx| {
            configure_guest_terminal(&mut state);
            let host = state.into_host();
            let started = ctl(json!({"cmd":"start","agent":"guest-terminal","isolation":"bare"}));
            let session = started["id"].as_str().unwrap().to_string();
            ctl(json!({"cmd":"plan_set","goal":"Kept across windows","tasks":[]}));

            // The first window: the board, the plan, and the shell's terminal, typed into under the lease.
            let mut first = ClientApp::new(ctx, Target::Local, None, Launch::default());
            until(&mut first, None, "the board", |app| {
                has_session(app, &session)
                    && app.driver().remote.with_mirror(|m| m.plans.values().any(|p| p.goal == "Kept across windows"))
            });
            until(&mut first, Some(&session), "the shell's screen", |app| {
                screen_text(app, &session).contains("terminal ready")
            });
            first.driver().remote.acquire(&session, false);
            until(&mut first, Some(&session), "the lease", |app| app.driver().remote.lease(&session).yours);
            first.driver().remote.type_text(&session, "first window\r").unwrap();
            until(&mut first, Some(&session), "the typed line's output", |app| {
                screen_text(app, &session).contains("received: first window")
            });
            let pid = pid_of(&session);
            assert!(alive(pid));

            // The window closes: the host keeps the agent's process and its terminal.
            drop(first);
            std::thread::sleep(Duration::from_millis(300));
            assert_eq!(pid_of(&session), pid, "the same process");
            assert!(alive(pid));

            // A new window shows the same session, its terminal as it was, and the plan.
            let mut second = ClientApp::new(ctx, Target::Local, None, Launch::default());
            until(&mut second, None, "the board again", |app| {
                has_session(app, &session)
                    && app.driver().remote.with_mirror(|m| m.plans.values().any(|p| p.goal == "Kept across windows"))
            });
            until(&mut second, Some(&session), "the terminal as it was", |app| {
                let text = screen_text(app, &session);
                text.contains("terminal ready") && text.contains("received: first window")
            });
            // And it types there too: the old window's lease ended with its connection.
            second.driver().remote.acquire(&session, false);
            until(&mut second, Some(&session), "the lease", |app| app.driver().remote.lease(&session).yours);
            second.driver().remote.type_text(&session, "second window\r").unwrap();
            until(&mut second, Some(&session), "the second window's output", |app| {
                screen_text(app, &session).contains("received: second window")
            });
            assert_eq!(pid_of(&session), pid);
            drop(second);
            drop(host);
        },
    );
}

#[test]
fn two_windows_share_one_board_and_one_lease() {
    with_state("app::client_tests::two_windows_share_one_board_and_one_lease", |mut state, ctx| {
        configure_guest_terminal(&mut state);
        let host = state.into_host();
        let started = ctl(json!({"cmd":"start","agent":"guest-terminal","isolation":"bare"}));
        let session = started["id"].as_str().unwrap().to_string();
        let (mut a, mut b) = (
            ClientApp::new(ctx, Target::Local, None, Launch::default()),
            ClientApp::new(ctx, Target::Local, None, Launch::default()),
        );
        for (name, app) in [("a", &mut a), ("b", &mut b)] {
            // As the views do: a terminal is followed once its session is on the board.
            until(app, None, &format!("the session in {name}"), |app| has_session(app, &session));
            until(app, Some(&session), &format!("the shell in {name}"), |app| {
                screen_text(app, &session).contains("terminal ready")
            });
        }
        a.driver().remote.acquire(&session, false);
        until(&mut a, Some(&session), "a's lease", |app| app.driver().remote.lease(&session).yours);
        until(&mut b, Some(&session), "b sees a typing", |app| {
            let lease = app.driver().remote.lease(&session);
            lease.holder.is_some() && !lease.yours
        });
        // b takes over, as its Take over button does; a's lease ends.
        b.driver().remote.acquire(&session, true);
        until(&mut b, Some(&session), "b's lease", |app| app.driver().remote.lease(&session).yours);
        until(&mut a, Some(&session), "a loses it", |app| !app.driver().remote.lease(&session).yours);
        b.driver().remote.type_text(&session, "from b\r").unwrap();
        for app in [&mut b, &mut a] {
            until(app, Some(&session), "both see b's line", |app| {
                screen_text(app, &session).contains("received: from b")
            });
        }
        // A plan set meanwhile reaches both boards.
        ctl(json!({"cmd":"plan_set","goal":"Seen by both","tasks":[]}));
        for app in [&mut a, &mut b] {
            until(app, None, "the plan", |app| {
                app.driver().remote.with_mirror(|m| m.plans.values().any(|p| p.goal == "Seen by both"))
            });
        }
        drop((a, b));
        drop(host);
    });
}

#[test]
fn a_window_that_stalls_catches_up_with_the_board() {
    with_state("app::client_tests::a_window_that_stalls_catches_up_with_the_board", |mut state, ctx| {
        state.config.remote.enabled = true;
        let host = state.into_host();
        let mut app = ClientApp::new(ctx, Target::Local, None, Launch::default());
        until(&mut app, None, "the board", |app| app.driver().remote.with_mirror(|m| m.epoch.is_some()));
        // The window draws nothing for a while (a long frame, a sleeping laptop's lid) while the host's
        // board changes many times: its link stops reading once its read-ahead is full.
        for n in 0..300 {
            ctl(json!({"cmd":"plan_set","goal":format!("Stalled {n}"),"tasks":[]}));
        }
        let goals: std::collections::BTreeSet<String> = (0..300).map(|n| format!("Stalled {n}")).collect();
        until(&mut app, None, "every plan", |app| {
            app.driver().remote.with_mirror(|m| goals.iter().all(|goal| m.plans.values().any(|p| &p.goal == goal)))
        });
        assert!(app.driver().remote.live(), "connected, after a resync or a reconnect");
        drop(app);
        drop(host);
    });
}

/// "Use this Mac": `[client] connect` leaves the config file that set it, with a backup beside it, and the
/// rest of the file and the main config stay as they were.
#[test]
fn using_this_mac_takes_the_connect_setting_out_of_the_config() {
    with_state("app::client_tests::using_this_mac_takes_the_connect_setting_out_of_the_config", |_state, _| {
        use atomics_core::config::Config;
        let [main, host] = Config::source_paths(&atomics_core::paths::config_file());
        let before_main = std::fs::read_to_string(&main).unwrap();
        std::fs::create_dir_all(host.parent().unwrap()).unwrap();
        std::fs::write(&host, "# this Mac\n[client]\nconnect = \"vm\"\n\n[layout]\nchief_dock = \"left\"\n").unwrap();
        assert_eq!(Config::load(&main).unwrap().client.connect, "vm");
        let changed = crate::client::forget_connect_setting().unwrap();
        assert_eq!(changed, std::slice::from_ref(&host));
        let after = std::fs::read_to_string(&host).unwrap();
        assert!(!after.contains("connect") && after.contains("chief_dock") && after.contains("# this Mac"), "{after}");
        assert_eq!(Config::load(&main).unwrap().client.connect, "", "the window is the host again");
        assert_eq!(std::fs::read_to_string(&main).unwrap(), before_main);
        let backups: Vec<_> = std::fs::read_dir(host.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".bak"))
            .collect();
        assert_eq!(backups.len(), 1, "a backup of the host file");
        assert!(crate::client::forget_connect_setting().unwrap().is_empty(), "nothing left to take out");
    });
}

/// A Mac's window draws the host in its VM, which has no desktop to confirm a Host command: Settings there still picks
/// the theme (`theme_pick`, with the window's session grant), and the window's own colours and egui's follow it, each
/// time it changes. Pinning works the same way. Other Host commands still need the host desktop.
#[test]
fn a_window_of_a_host_with_no_desktop_picks_the_theme_and_takes_it() {
    with_state(
        "app::client_tests::a_window_of_a_host_with_no_desktop_picks_the_theme_and_takes_it",
        |mut state, ctx| {
            use atomics_core::ops::HostAction;
            use atomics_ui::{Backend as _, Command};
            state.config.remote.enabled = true;
            // No desktop, as the VM's host runs (`atomics host`).
            state.window = crate::host::Window::default();
            let host = state.into_host();
            let mut app = ClientApp::new(ctx, Target::Local, None, Launch::default());
            until(&mut app, None, "the host's settings", |app| {
                app.driver().remote.with_mirror(|m| m.epoch.is_some() && m.host.management.is_some())
            });
            let draw = |app: &mut ClientApp| {
                app.pump();
                let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0));
                let mut output =
                    ctx.run_ui(egui::RawInput { screen_rect: Some(screen), ..Default::default() }, |ui| app.draw(ui));
                output.textures_delta.clear();
            };
            draw(&mut app);
            let pick = |name: &str| Command::Manage(HostAction::SelectTheme { name: name.into(), host: false });
            assert!(app.driver().remote.available(&pick("paper")), "Settings offers the theme");
            let grant =
                Command::Manage(HostAction::SettingsTool { tool: atomics_core::ops::SettingsTool::CheckSandbox });
            assert!(!app.driver().remote.available(&grant), "other settings still need the host desktop");
            for name in ["paper", "midnight", "paper"] {
                app.driver().remote.send(pick(name));
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(20));
                while host.state().config.theme.name != name {
                    assert!(Instant::now() < deadline, "the host never took {name}");
                    draw(&mut app);
                    std::thread::sleep(Duration::from_millis(10));
                }
                let wanted = host.state().theme.palette();
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(20));
                while app.palette().background != wanted.background {
                    assert!(Instant::now() < deadline, "the window never took {name}");
                    draw(&mut app);
                    std::thread::sleep(Duration::from_millis(10));
                }
                draw(&mut app);
                assert_eq!(app.palette().accent, wanted.accent, "{name}");
                let style = ctx.global_style();
                assert_eq!(style.visuals.panel_fill, app.palette().style().visuals.panel_fill, "egui draws {name} too");
                assert_eq!(style.visuals.dark_mode, app.palette().style().visuals.dark_mode, "{name}");
            }
            app.driver().remote.send(Command::Manage(HostAction::PinTheme { pinned: true }));
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(20));
            while host.state().store.pinned_theme.as_deref() != Some("paper") {
                assert!(Instant::now() < deadline, "the host never pinned the theme");
                draw(&mut app);
                std::thread::sleep(Duration::from_millis(10));
            }
            drop(app);
            drop(host);
        },
    );
}
