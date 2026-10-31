//! Connections against the fake `sbx`, a stand-in browser, fixture config files and the test
//! keychain. Nothing here reads a real Claude Code or Codex config or reaches the real sbx.
use super::super::tests::{request, with_app};
use super::*;
use crate::connections::tests::{browser, opened};
use crate::sandbox::fake;
use atomics_core::isolation::{self, IsolationMode};

type Texts = Vec<(String, Pos2)>;

fn texts(output: egui::FullOutput) -> Texts {
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

fn click(pos: Pos2, pressed: bool) -> Vec<egui::Event> {
    vec![
        egui::Event::PointerMoved(pos),
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        },
    ]
}

fn has(texts: &Texts, text: &str) -> bool {
    texts.iter().any(|(shown, _)| shown == text)
}

/// The text `label` on the same line as `row`, such as a row's Connect button.
fn beside(texts: &Texts, row: &str, label: &str) -> Pos2 {
    let y = texts.iter().find(|(text, _)| text == row).unwrap_or_else(|| panic!("{row} in {texts:?}")).1.y;
    texts
        .iter()
        .filter(|(text, _)| text == label)
        .min_by(|a, b| (a.1.y - y).abs().total_cmp(&(b.1.y - y).abs()))
        .unwrap_or_else(|| panic!("{label} in {texts:?}"))
        .1
}

/// Draws Settings → Connections and applies what it asks for.
fn panel(app: &mut Desktop<'_>, ctx: &egui::Context, events: Vec<egui::Event>) -> Texts {
    let mut action = None;
    let output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(900.0, 1100.0))),
            time: Some(ctx.input(|input| input.time) + 0.1),
            events,
            ..Default::default()
        },
        |ui| {
            egui::CentralPanel::default().show(ui, |ui| action = app.connections_panel(ui));
        },
    );
    if let Some(action) = action {
        app.connection_action(action);
    }
    app.runtime.drain_actions();
    texts(output)
}

fn click_panel(app: &mut Desktop<'_>, ctx: &egui::Context, pos: Pos2) {
    for pressed in [true, false] {
        panel(app, ctx, click(pos, pressed));
    }
}

/// Draws one session's tile.
fn tile(app: &mut Desktop<'_>, ctx: &egui::Context, index: usize, events: Vec<egui::Event>) -> Texts {
    let output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1400.0, 900.0))),
            time: Some(ctx.input(|input| input.time) + 0.1),
            events,
            ..Default::default()
        },
        |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let layout = layout::compute(1400.0, 900.0, 1, 0, false, 0.0, &app.runtime.config.layout);
                app.tiles(ui, ui.max_rect(), &layout, &[index]);
            });
        },
    );
    app.runtime.drain_actions();
    texts(output)
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

fn idle(app: &mut Desktop<'_>) -> bool {
    app.runtime.connections.jobs.is_empty() && app.runtime.connections.busy.is_empty()
}

/// A fake sbx in `bin`, a stand-in browser beside it, and config files that exist only in `configs`.
fn machine(app: &mut Desktop<'_>, bin: &Path, configs: &Path) -> String {
    let program = fake::install(bin).display().to_string();
    app.runtime.sbx = program.clone();
    app.runtime.connections.browser = Some(browser(bin));
    let dirs = |name: &str| match name {
        "CLAUDE_CONFIG_DIR" => Some(configs.join("claude").display().to_string()),
        "CODEX_HOME" => Some(configs.join("codex").display().to_string()),
        _ => None,
    };
    let mut sources = crate::connections::Sources::at(configs, cfg!(target_os = "macos"), dirs);
    sources.files[1].1 = configs.join("managed-mcp.json");
    app.runtime.connections.sources = Some(sources);
    program
}

fn register(program: &str, name: &str, url: &str) {
    crate::sandbox::sbx(program, &isolation::connection_url_args(name, url), false, Duration::from_secs(60)).unwrap();
}

/// The MCP registrations, authorizations, loads and removals sbx was asked for.
fn mcp_changes(bin: &Path) -> Vec<Vec<String>> {
    fake::calls_to(bin, &["mcp"])
        .into_iter()
        .filter(|call| matches!(call[1].as_str(), "add" | "auth" | "load" | "unload" | "rm"))
        .collect()
}

/// A shell stand-in that stays attached, started sandboxed in a scratch folder with `prompt`.
fn sandboxed(app: &mut Desktop<'_>, ctx: &egui::Context, bin: &Path, prompt: &str) -> (String, String) {
    std::fs::write(bin.join("run-waits"), "wait").unwrap();
    app.runtime.config.isolation.mode = IsolationMode::Sandbox;
    if app.runtime.config.agent("probe").is_none() {
        app.runtime.config.agents.push(atomics_core::config::AgentConfig {
            name: "probe".into(),
            command: vec!["/bin/sleep".into(), "600".into()],
            kind: AgentKind::Shell,
            busy_text: None,
            isolation: None,
        });
    }
    let response = request(app, json!({"cmd":"start","agent":"probe","prompt":prompt}));
    assert!(response.ok, "{:?}", response.error);
    let id = response.result["id"].as_str().unwrap().to_string();
    tick_until(app, ctx, "the sandbox to attach", |app| {
        app.runtime.sessions.iter().any(|s| s.id == id && s.terminal.is_some())
    });
    let name = isolation::sandbox_name(&id);
    (id, name)
}

#[test]
fn settings_import_adds_nothing_until_a_click_connects_and_disconnects() {
    with_app(
        "app::connections::tests::settings_import_adds_nothing_until_a_click_connects_and_disconnects",
        |app, ctx| {
            let bin = crate::test_support::tempdir().unwrap();
            let configs = crate::test_support::tempdir().unwrap();
            let program = machine(app, bin.path(), configs.path());
            std::fs::write(bin.path().join("auth-free-sentry"), "").unwrap();
            register(&program, "sentry", "https://mcp.sentry.dev/mcp");
            std::fs::create_dir_all(configs.path().join("claude")).unwrap();
            std::fs::write(
                configs.path().join("claude/.claude.json"),
                r#"{"oauthAccount": {"emailAddress": "PRIVATE-account"}, "history": ["PRIVATE-history"],
                "mcpServers": {
                  "notion": {"type": "http", "url": "https://mcp.notion.com/mcp"},
                  "fs": {"command": "npx", "args": ["-y", "server-filesystem"], "env": {"FS_TOKEN": "PRIVATE-fs"}}}}"#,
            )
            .unwrap();
            std::fs::create_dir_all(configs.path().join("codex")).unwrap();
            std::fs::write(
                configs.path().join("codex/config.toml"),
                "model = 'PRIVATE-model'\n[mcp_servers.playwright]\ncommand = \"npx\"\nargs = [\"@playwright/mcp\"]\n",
            )
            .unwrap();

            // Opening the panel reads sbx's servers.
            panel(app, ctx, vec![]);
            tick_until(app, ctx, "sbx's servers", |app| app.runtime.connections.registered.is_some() && idle(app));
            let shown = panel(app, ctx, vec![]);
            assert!(has(&shown, "sentry") && has(&shown, "connected"), "{shown:?}");

            let import = beside(&shown, "Import from Claude Code / Codex", "Import from Claude Code / Codex");
            click_panel(app, ctx, import);
            tick_until(app, ctx, "the import", |app| !app.runtime.connections.importing && idle(app));
            assert!(mcp_changes(bin.path()).len() == 1, "Import registers nothing: {:?}", mcp_changes(bin.path()));
            let shown = panel(app, ctx, vec![]);
            for name in ["fs", "notion", "playwright"] {
                assert!(has(&shown, name), "{name} in {shown:?}");
            }
            assert_eq!(shown.iter().filter(|(text, _)| text == "not connected").count(), 3);
            assert!(has(&shown, "Connect and store 1 env values") && has(&shown, "Connect without them"));
            assert!(!format!("{shown:?}").contains("PRIVATE"), "{shown:?}");

            // One click: sbx registers notion, its sign-in opens in the browser, and it is connected.
            click_panel(app, ctx, beside(&shown, "notion", "Connect"));
            tick_until(app, ctx, "notion to connect", idle);
            assert_eq!(opened(bin.path()).len(), 1);
            assert_eq!(fake::calls_to(bin.path(), &["mcp", "auth"]), [["mcp", "auth", "notion"]]);
            tick_until(app, ctx, "the refresh", |app| {
                app.runtime.connections.status("notion") == Some(&Status::Connected)
            });

            // A command server whose environment the user chose to store.
            let shown = panel(app, ctx, vec![]);
            click_panel(app, ctx, beside(&shown, "fs", "Connect and store 1 env values"));
            tick_until(app, ctx, "fs to connect", idle);
            let store = crate::credentials::Store::new(app.runtime.relay.keychain().clone());
            assert_eq!(store.mcp_env("fs").unwrap().unwrap()["FS_TOKEN"], "PRIVATE-fs");
            let add = fake::calls_to(bin.path(), &["mcp", "add"]).into_iter().find(|call| call[2] == "fs").unwrap();
            assert!(add.contains(&"--args=--env".to_string()) && add.contains(&"--args=mcp-host".to_string()));
            assert_eq!(opened(bin.path()).len(), 1, "a command server needs no sign-in");
            assert!(!std::fs::read_to_string(bin.path().join("calls.log")).unwrap().contains("PRIVATE"));
            tick_until(app, ctx, "the refresh", |app| app.runtime.connections.status("fs") == Some(&Status::Connected));

            let shown = panel(app, ctx, vec![]);
            click_panel(app, ctx, beside(&shown, "notion", "Disconnect"));
            tick_until(app, ctx, "notion to go", |app| {
                idle(app) && app.runtime.connections.status("notion").is_none() && !app.runtime.connections.refreshing
            });
            assert_eq!(fake::calls_to(bin.path(), &["mcp", "rm"]), [["mcp", "rm", "--force", "notion"]]);
            let shown = panel(app, ctx, vec![]);
            assert!(has(&shown, "notion"), "its definition stays, ready to connect again");
            assert!(!format!("{shown:?} {:?}", app.runtime.toast).contains("PRIVATE"));
        },
    );
}

#[test]
fn sandboxed_sessions_get_connected_servers_and_a_banner_for_the_rest() {
    with_app(
        "app::connections::tests::sandboxed_sessions_get_connected_servers_and_a_banner_for_the_rest",
        |app, ctx| {
            let bin = crate::test_support::tempdir().unwrap();
            let configs = crate::test_support::tempdir().unwrap();
            let program = machine(app, bin.path(), configs.path());
            std::fs::write(bin.path().join("auth-free-sentry"), "").unwrap();
            register(&program, "sentry", "https://mcp.sentry.dev/mcp");
            register(&program, "slack", "https://mcp.slack.com/mcp");

            // An Ungrouped scratch session: it gets every connected server, and a banner for slack.
            let (id, name) = sandboxed(app, ctx, bin.path(), "Summarize the channel");
            let index = app.runtime.find(&id).unwrap();
            assert!(app.runtime.sessions[index].scratch.is_some(), "an Ungrouped session");
            assert_eq!(
                fake::calls_to(bin.path(), &["mcp", "load"]),
                [isolation::mcp_load_args(&name, &name), isolation::mcp_load_args("sentry", &name)]
            );
            assert_eq!(app.runtime.sessions[index].mcp_needed, [Needed::missing("slack".into())]);
            // The banner reads sbx's status by itself, without Settings → Connections ever opening.
            tick_until(app, ctx, "sbx's servers", |app| idle(app) && app.runtime.connections.registered.is_some());
            app.runtime.connections.busy.insert("slack".into());
            assert!(app.runtime.restart_blockers().iter().any(|b| b == "MCP connection under way: slack"));
            app.runtime.connections.busy.clear();
            let shown = tile(app, ctx, index, vec![]);
            assert!(has(&shown, "Connect slack") && has(&shown, "registered with sbx, needs sign-in"), "{shown:?}");
            let pos = beside(&shown, "Connect slack", "Connect slack");
            for pressed in [true, false] {
                tile(app, ctx, index, click(pos, pressed));
            }
            tick_until(app, ctx, "slack to connect", idle);
            assert_eq!(fake::calls_to(bin.path(), &["mcp", "auth"]), [["mcp", "auth", "slack"]]);
            assert!(opened(bin.path())[0].starts_with("https://auth.example.com/authorize?"));
            assert_eq!(std::fs::read_to_string(bin.path().join("auth-no-browser")).unwrap().trim(), "1");
            assert_eq!(
                fake::calls_to(bin.path(), &["mcp", "load"]).last().unwrap(),
                &isolation::mcp_load_args("slack", &name)
            );
            let index = app.runtime.find(&id).unwrap();
            assert!(app.runtime.sessions[index].mcp_needed.is_empty());
            assert!(!has(&tile(app, ctx, index, vec![]), "Connect slack"));
            assert!(!format!("{:?}", app.runtime.toast).contains("PRIVATE"));

            // The next session gets slack too, now that it is connected.
            let (second, other) = sandboxed(app, ctx, bin.path(), "Post the summary");
            assert!(fake::calls_to(bin.path(), &["mcp", "load"]).contains(&isolation::mcp_load_args("slack", &other)));
            assert!(app.runtime.sessions[app.runtime.find(&second).unwrap()].mcp_needed.is_empty());
        },
    );
}

#[test]
fn aliases_are_explained_and_failed_loads_keep_the_retry_banner() {
    with_app("app::connections::tests::aliases_are_explained_and_failed_loads_keep_the_retry_banner", |app, ctx| {
        let bin = crate::test_support::tempdir().unwrap();
        let configs = crate::test_support::tempdir().unwrap();
        let program = machine(app, bin.path(), configs.path());
        let url = "https://mcp.example.com/mcp";
        for name in ["service", "service-alias"] {
            std::fs::write(bin.path().join(format!("auth-free-{name}")), "").unwrap();
            register(&program, name, url);
        }
        let (id, sandbox) = sandboxed(app, ctx, bin.path(), "Check aliases");
        app.runtime.refresh_connections();
        tick_until(app, ctx, "the connections list", idle);
        let shown = panel(app, ctx, vec![]);
        assert!(
            shown.iter().any(|(text, _)| text.contains("same URL as service; tools loaded once per sandbox")),
            "{shown:?}"
        );
        assert_eq!(
            fake::calls_to(bin.path(), &["mcp", "load"]),
            [isolation::mcp_load_args(&sandbox, &sandbox), isolation::mcp_load_args("service", &sandbox)]
        );
        let index = app.runtime.find(&id).unwrap();
        assert!(app.runtime.sessions[index].mcp_needed.is_empty());

        std::fs::write(bin.path().join("auth-free-unrelated"), "").unwrap();
        register(&program, "unrelated", "https://other.example.com/mcp");
        app.runtime.sessions[index].mcp_needed.push(Needed::missing("unrelated".into()));
        std::fs::write(bin.path().join("fail-mcp-load"), "").unwrap();
        app.runtime.connect_server("unrelated", false, None);
        tick_until(app, ctx, "the failed load", idle);
        let index = app.runtime.find(&id).unwrap();
        assert_eq!(app.runtime.sessions[index].mcp_needed, [Needed::missing("unrelated".into())]);
        assert!(app.runtime.toast.as_ref().unwrap().0.contains("retry Connect"));
        std::fs::remove_file(bin.path().join("fail-mcp-load")).unwrap();
        app.runtime.connect_server("unrelated", false, None);
        tick_until(app, ctx, "the successful retry", idle);
        assert!(app.runtime.sessions[index].mcp_needed.is_empty());
    });
}

#[test]
fn an_agents_request_only_raises_the_banner_and_leaves_sbx_alone() {
    with_app("app::connections::tests::an_agents_request_only_raises_the_banner_and_leaves_sbx_alone", |app, ctx| {
        let bin = crate::test_support::tempdir().unwrap();
        let configs = crate::test_support::tempdir().unwrap();
        let program = machine(app, bin.path(), configs.path());
        std::fs::write(bin.path().join("auth-free-sentry"), "").unwrap();
        register(&program, "sentry", "https://mcp.sentry.dev/mcp");
        let (id, name) = sandboxed(app, ctx, bin.path(), "Use Notion");
        app.runtime.refresh_connections();
        tick_until(app, ctx, "sbx's servers", idle);
        let before = (mcp_changes(bin.path()), crate::connections::registered(&program).unwrap());

        let url = "https://mcp.notion.example/mcp?workspace=team-a";
        for server in ["notion", "sentry"] {
            let asked = request(app, json!({"cmd":"mcp_connect","pane":name,"server":server,"url":url}));
            assert!(asked.ok && asked.result["asked"] == true, "{:?}", asked.error);
        }
        for (pane, server, url) in [
            (Some(name.as_str()), "notion", "http://mcp.notion.example/mcp"),
            (Some(name.as_str()), "Notion", url),
            (None, "notion", url),
        ] {
            assert!(!request(app, json!({"cmd":"mcp_connect","pane":pane,"server":server,"url":url})).ok);
        }
        tick_until(app, ctx, "any work an agent's request started", idle);
        assert_eq!((mcp_changes(bin.path()), crate::connections::registered(&program).unwrap()), before);
        let index = app.runtime.find(&id).unwrap();
        assert_eq!(
            app.runtime.sessions[index].mcp_needed,
            [
                Needed { name: "notion".into(), url: Some(url.into()), asked: true },
                // sentry is known: the agent's URL never replaces its registration.
                Needed { name: "sentry".into(), url: None, asked: true },
            ]
        );

        // The banner shows who asked and the URL in full; only the user's click connects it.
        let shown = tile(app, ctx, index, vec![]);
        assert!(has(&shown, &format!("asked by Use Notion: {url}")), "{shown:?}");
        let pos = beside(&shown, "Connect notion", "Connect notion");
        for pressed in [true, false] {
            tile(app, ctx, index, click(pos, pressed));
        }
        tick_until(app, ctx, "notion to connect", idle);
        assert_eq!(
            fake::calls_to(bin.path(), &["mcp", "add"]).last().unwrap(),
            &isolation::connection_url_args("notion", url)
        );
        assert_eq!(opened(bin.path()).len(), 1);
        assert_eq!(
            fake::calls_to(bin.path(), &["mcp", "load"]).last().unwrap(),
            &isolation::mcp_load_args("notion", &name)
        );

        // A bare agent uses its own MCP configuration.
        let bare = request(app, json!({"cmd":"start","agent":"probe","isolation":"bare"}));
        let bare = bare.result["id"].as_str().unwrap().to_string();
        let refused = request(app, json!({"cmd":"mcp_connect","pane":bare,"server":"notion"}));
        assert!(!refused.ok && refused.error.unwrap().contains("bare"));
    });
}
