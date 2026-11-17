use super::tests::{wait_started, with_app};
use super::*;
use atomics_core::config::ChiefAgent;
use atomics_core::projection::{ChiefAuth, ChiefProvider};

#[test]
fn chief_provider_launch_switch_and_history() {
    with_app("app::chief_provider_tests::chief_provider_launch_switch_and_history", |app, _| {
        app.runtime.chief_probe_at = Some(Instant::now());
        let root = app.runtime.launch_dir.clone();
        let fake = crate::test_support::tempdir().unwrap();
        for agent in [ChiefAgent::Claude, ChiefAgent::Codex] {
            let program = fake.path().join(agent.name());
            crate::test_support::write_program(
                &program,
                r#"#!/usr/bin/env python3
import json, os, sys, time
if "--version" in sys.argv:
    print("codex-cli 0.169.0")
    sys.exit()
with open(os.path.basename(sys.argv[0]) + "-launch.json", "w") as f:
    json.dump(sys.argv[1:], f)
time.sleep(60)
"#,
            );
            let configured = app.runtime.config.agents.iter_mut().find(|a| a.name == agent.name()).unwrap();
            configured.command = vec![program.to_string_lossy().into_owned()];
            app.runtime.chief_providers.push(ChiefProvider {
                agent,
                installed: true,
                auth: ChiefAuth::SignedIn,
                models: match agent {
                    ChiefAgent::Claude => vec!["sonnet".into()],
                    _ => vec!["gpt-6.2-sol".into(), "gpt-6.2-astra".into()],
                },
                ..Default::default()
            });
        }
        app.runtime.config.chief = toml::from_str("agent='codex'\nnative_messaging='refuse'").unwrap();
        app.runtime.config.chief.cwd = Some(root.to_string_lossy().into_owned());
        let read = |agent: &str| -> Vec<String> {
            let path = root.join(format!("{agent}-launch.json"));
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Ok(args) = std::fs::read(&path)
                    && let Ok(args) = serde_json::from_slice(&args)
                {
                    std::fs::remove_file(&path).unwrap();
                    return args;
                }
                assert!(Instant::now() < deadline, "missing {agent} launch");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let state = &mut *app.runtime;
        let initial = state.start_chief(None).unwrap();
        state.toggle_chief();
        assert_eq!(
            state.sessions.iter().find(|s| s.chief).unwrap().id,
            initial,
            "opening the panel must preserve a preparing chief"
        );
        wait_started(state, &initial);
        let args = read("codex");
        let joined = args.join("\n");
        for expected in [
            "--chief",
            "developer_instructions=",
            "hooks.PermissionRequest",
            "hooks.UserPromptSubmit",
            "hooks.Stop",
            "gpt-6.2-sol",
            "default_tools_approval_mode=\"prompt\"",
            "tools.atomics_plan_set.approval_mode=\"approve\"",
        ] {
            assert!(joined.contains(expected), "missing {expected}");
        }
        assert!(!joined.contains("--last"));
        assert!(!joined.contains("ATOMICS_NOTE_CAPABILITY ="));
        assert!(state.set_chief_model(ChiefAgent::Codex, "gpt-6.2-astra", Some(&initial), false).is_err());
        let hook = |pane: &str, event: &str, text: &str| HookMessage {
            pane: Some(pane.into()),
            ancestors: vec![],
            approval_reply: None,
            delivered: 0,
            event: atomics_core::hook::HookEvent {
                name: event.into(),
                session_id: "root-thread".into(),
                prompt: Some(text.into()),
                last_assistant_message: Some(text.into()),
                ..Default::default()
            },
        };
        state.on_hook(hook(&initial, "SessionStart", ""));
        state.on_hook(hook(&initial, "UserPromptSubmit", "Review the demo."));
        assert!(state.set_chief_model(ChiefAgent::Codex, "gpt-6.2-astra", Some(&initial), false).is_err());
        state.on_hook(hook(&initial, "Stop", "The demo is ready."));
        let resumed = state.set_chief_model(ChiefAgent::Codex, "gpt-6.2-astra", Some(&initial), false).unwrap();
        wait_started(state, &resumed);
        assert!(read("codex").windows(2).any(|w| w == ["resume", "root-thread"]));
        state.on_hook(hook(&initial, "UserPromptSubmit", "late old process"));
        assert_eq!(state.sessions.len(), 1);
        state.on_hook(hook(&resumed, "SessionStart", ""));
        // Valid notebook IDs can be all digits and fall outside generic credential heuristics.
        let capability = "00000000-0000-4000-8000-000000000001";
        std::fs::write(paths::atomics_home().join("notebook-auth").join(&resumed), capability).unwrap();
        state.plans.push(Plan { id: "demo".into(), goal: format!("Review {capability}"), ..Default::default() });
        assert!(!state.chief_handoff(Some("root-thread"), ChiefAgent::Codex).contains(capability));
        let next = state.set_chief_model(ChiefAgent::Claude, "sonnet", Some(&resumed), true).unwrap();
        wait_started(state, &next);
        let args = read("claude");
        assert!(!args.contains(&"--resume".into()));
        assert!(args.contains(&"--allowedTools".into()));
        assert!(!args.join(" ").contains(&crate::notebook_auth::read(&paths::atomics_home(), &next).unwrap()));
        let handoff = args.last().unwrap();
        assert!(handoff.contains("ATOMICS data") && handoff.contains("The demo is ready."));
        assert!(handoff.contains("demo") && !handoff.contains(capability));
        assert!(!handoff.contains("late old process"));
        let history = serde_json::to_string(&state.store.chief_history).unwrap();
        assert!(history.contains("Review the demo.") && history.contains("The demo is ready."));
        assert!(state.set_chief_model(ChiefAgent::Codex, "gpt-6.2-sol", Some(&resumed), true).is_err());
    });
}

#[test]
fn chief_provider_discovery_reads_opencode_without_projecting_auth_output() {
    with_app(
        "app::chief_provider_tests::chief_provider_discovery_reads_opencode_without_projecting_auth_output",
        |app, _| {
            let state = &mut *app.runtime;
            let fixture = crate::test_support::tempdir().unwrap();
            let program = fixture.path().join("opencode");
            crate::test_support::write_program(
                &program,
                "#!/bin/sh\ncase \"$1\" in\nauth) echo fixture-private-account; echo '└  10 credentials';;\nmodels) echo demo/local;;\nesac\n",
            );
            state.config.chief = toml::from_str("agent='opencode'").unwrap();
            state.config.chief.command = vec![program.to_string_lossy().into_owned()];
            state.chief_probe_at = None;
            state.refresh_chief_providers();
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                state.runtime_tick();
                let choices = state.available_chiefs();
                let row = choices.providers.iter().find(|r| r.agent == ChiefAgent::Opencode).unwrap();
                if row.installed {
                    assert_eq!(row.auth, ChiefAuth::SignedIn);
                    assert_eq!(row.models, ["demo/local"]);
                    assert_eq!(row.unavailable_reason.as_deref(), Some(ChiefAgent::OPENCODE_UNAVAILABLE));
                    assert!(!serde_json::to_string(&choices).unwrap().contains("fixture-private-account"));
                    break;
                }
                assert!(Instant::now() < deadline, "provider discovery did not finish");
                std::thread::sleep(Duration::from_millis(10));
            }
        },
    );
}

#[test]
fn chief_provider_projection_distinguishes_auth_and_remaining_quota() {
    with_app(
        "app::chief_provider_tests::chief_provider_projection_distinguishes_auth_and_remaining_quota",
        |app, _| {
            let state = &mut *app.runtime;
            state.chief_providers = vec![ChiefProvider {
                agent: ChiefAgent::Codex,
                installed: true,
                auth: ChiefAuth::SignedOut,
                models: vec!["gpt-6.2-sol".into()],
                ..Default::default()
            }];
            state.usage.codex.session = Some(atomics_core::usage::Window { left_percent: 0., resets_at: i64::MAX });
            let data = state.available_chiefs();
            let codex = data.providers.iter().find(|r| r.agent == ChiefAgent::Codex).unwrap();
            assert_eq!(codex.auth, ChiefAuth::SignedOut);
            assert!(serde_json::to_value(&data).unwrap().get("history").is_none());
            assert_eq!(codex.quota.session.as_ref().unwrap().left_percent, 0.);
            assert!(codex.unavailable_reason.is_some());
            state.chief_providers[0].auth = ChiefAuth::Unknown;
            state.usage.codex.session.as_mut().unwrap().resets_at = 1;
            let data = state.available_chiefs();
            let codex = data.providers.iter().find(|r| r.agent == ChiefAgent::Codex).unwrap();
            assert_eq!(codex.auth, ChiefAuth::Unknown);
            assert!(codex.quota.session.is_none());
            assert_eq!(
                data.providers.iter().find(|r| r.agent == ChiefAgent::Opencode).unwrap().unavailable_reason.as_deref(),
                Some("opencode chief needs permission and trace integration")
            );
            state.config.chief.agent = ChiefAgent::Opencode;
            state.config.chief.cwd = Some(state.launch_dir.to_string_lossy().into_owned());
            assert_eq!(state.start_chief(None).unwrap_err(), ChiefAgent::OPENCODE_UNAVAILABLE);
            state.config.chief.agent = ChiefAgent::Codex;
            state.config.chief.resume = Some("--last".into());
            assert!(state.start_chief(None).is_err());
        },
    );
}
