use super::tests::{session, wait_started, with_app};
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
if "gpt-6.2-rejected" in sys.argv:
    print("model rejected by test provider", flush=True)
    sys.exit(1)
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
                    _ => vec!["gpt-6.2-sol".into(), "gpt-6.2-astra".into(), "gpt-6.2-rejected".into()],
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
            "--sandbox",
            "read-only",
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

        let cancelled = state.set_chief_model(ChiefAgent::Codex, "gpt-6.2-astra", Some(&initial), false).unwrap();
        state.close_session(&cancelled, false);
        assert!(!state.available_chiefs().switching);
        let rejected = state.set_chief_model(ChiefAgent::Codex, "gpt-6.2-rejected", Some(&initial), false).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while state.available_chiefs().switching {
            assert!(Instant::now() < deadline, "replacement failure was never processed");
            state.runtime_tick();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(state.sessions.iter().all(|s| s.id != rejected));
        assert!(state.toast.as_ref().unwrap().0.contains("model rejected by test provider"));
        assert_eq!(state.chief_session_id(), Some(initial.as_str()));
        assert!(state.store.chief_selection.is_none());
        let _ = read("codex");

        let interrupted = state.set_chief_model(ChiefAgent::Codex, "gpt-6.2-astra", Some(&initial), false).unwrap();
        wait_started(state, &interrupted);
        let _ = read("codex");
        let mut permission = hook(&initial, "PermissionRequest", "");
        permission.approval_reply = Some(Arc::new(ipc::ApprovalReply::default()));
        state.on_hook(permission);
        state.on_hook(hook(&interrupted, "SessionStart", ""));
        state.runtime_tick();
        assert_eq!(state.chief_session_id(), Some(initial.as_str()));
        assert_eq!(state.sessions[0].approvals.len(), 1);
        assert!(!state.available_chiefs().switching);
        state.on_hook(hook(&initial, "Stop", "No changes pending."));
        let resumed = state.set_chief_model(ChiefAgent::Codex, "gpt-6.2-astra", Some(&initial), false).unwrap();
        wait_started(state, &resumed);
        assert!(read("codex").windows(2).any(|w| w == ["resume", "root-thread"]));
        assert_eq!(state.chief_session_id(), Some(initial.as_str()));
        state.on_hook(hook(&resumed, "SessionStart", ""));
        state.runtime_tick();
        assert_eq!(state.chief_session_id(), Some(resumed.as_str()));
        state.on_hook(hook(&initial, "UserPromptSubmit", "late old process"));
        assert_eq!(state.sessions.len(), 1);
        // Valid notebook IDs can be all digits and fall outside generic credential heuristics.
        let capability = "00000000-0000-4000-8000-000000000001";
        std::fs::write(paths::atomics_home().join("notebook-auth").join(&resumed), capability).unwrap();
        state.plans.push(Plan { id: "demo".into(), goal: format!("Review {capability}"), ..Default::default() });
        assert!(!state.chief_handoff(Some("root-thread"), ChiefAgent::Codex).contains(capability));
        state.config.chief = toml::from_str("agent='claude'\nmodel='sonnet'").unwrap();
        state.config.chief.command = vec![fake.path().join("claude").to_string_lossy().into_owned()];
        assert_eq!(state.available_chiefs().selected.agent, ChiefAgent::Codex);
        assert!(matches!(state.chief_author(), atomics_core::tickets::Author::Chief(label) if label.contains("Astra")));
        let next = state.set_chief_model(ChiefAgent::Claude, "sonnet", Some(&resumed), true).unwrap();
        assert_ne!(next, resumed);
        wait_started(state, &next);
        let args = read("claude");
        assert!(!args.contains(&"--resume".into()));
        assert!(args.contains(&"--allowedTools".into()));
        assert!(!args.join(" ").contains(&crate::notebook_auth::read(&paths::atomics_home(), &next).unwrap()));
        let claude_id = args.windows(2).find(|arg| arg[0] == "--session-id").unwrap()[1].clone();
        let mut late_reply = hook(&resumed, "Stop", "Final result after replacement spawn");
        late_reply.event.last_assistant_message = Some("Final result after replacement spawn".into());
        state.on_hook(late_reply);
        let mut ready = hook(&next, "SessionStart", "");
        ready.event.session_id = claude_id.clone();
        state.on_hook(ready);
        state.runtime_tick();
        let chief = state.sessions.iter().find(|s| s.id == next).unwrap();
        let handoff = &chief.pending.as_ref().unwrap().text;
        assert!(handoff.contains("ATOMICS data") && handoff.contains("The demo is ready."));
        assert!(handoff.contains("Final result after replacement spawn"));
        assert!(handoff.contains("demo") && !handoff.contains(capability));
        assert!(!handoff.contains("late old process"));
        let history = serde_json::to_string(&state.store.chief_history).unwrap();
        assert!(history.contains("Review the demo.") && history.contains("The demo is ready."));
        assert!(state.set_chief_model(ChiefAgent::Codex, "gpt-6.2-sol", Some(&resumed), true).is_err());

        let retired =
            state.store.sessions.iter().find(|record| record.chief && record.agent == "codex").unwrap().clone();
        state.repos.push(root.clone());
        assert!(atomics_ui::Backend::recent_sessions(&state.backend()).iter().all(|row| !row.record.chief));
        state.resume_record(retired);
        assert_eq!(state.sessions.iter().filter(|s| s.chief).count(), 1);
        let handoff = state.chief_handoff(Some("root-thread"), ChiefAgent::Codex);
        let long = format!("{handoff}{}", " historical detail".repeat(80));
        let prompt = sessions::chief_prompt_input(&next, &long, ChiefAgent::Codex).unwrap();
        assert!(prompt.contains("historical data") && !prompt.contains("follow it exactly"));
        assert_eq!(std::fs::read_to_string(paths::briefs_dir().join(format!("{next}.md"))).unwrap(), long);
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
                "#!/bin/sh\ncase \"$1\" in\nauth) echo fixture-private-account; echo '└  10 credentials';;\nmodels) echo demo/local; echo /private/credentials.json; echo demo/credential-file >&2;;\nesac\n",
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
            state.config.chief.agent = ChiefAgent::Codex;
            state.config.chief.model = Some("sk-proj-fixture-private-token".into());
            state.usage.codex.plan = Some("/private/credentials.json".into());
            state.usage.codex.models.insert(
                "gpt-6.2-sol".into(),
                atomics_core::usage::ModelUsage {
                    label: "/private/quota.json".into(),
                    weekly: Some(atomics_core::usage::Window { left_percent: 20., resets_at: i64::MAX }),
                    ..Default::default()
                },
            );
            state.usage.codex.session = Some(atomics_core::usage::Window { left_percent: 0., resets_at: i64::MAX });
            let data = state.available_chiefs();
            let codex = data.providers.iter().find(|r| r.agent == ChiefAgent::Codex).unwrap();
            assert_eq!(codex.auth, ChiefAuth::SignedOut);
            assert!(codex.quota.plan.is_none());
            assert!(data.selected.model.is_none());
            assert_eq!(codex.quota.models["gpt-6.2-sol"].label, "gpt-6.2-sol");
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

#[test]
fn chief_root_events_require_the_bound_pane_and_root() {
    with_app("app::chief_provider_tests::chief_root_events_require_the_bound_pane_and_root", |app, _| {
        let state = &mut *app.runtime;
        let root = state.launch_dir.clone();
        let mut chief = session("root", AgentKind::Generic, AgentStatus::Idle, &root);
        chief.chief = true;
        state.sessions.push(chief);
        for (pane, event, id, thread) in [
            (None, "UserPromptSubmit", "root", atomics_core::hook::HookThread::Main),
            (Some("wrong"), "Stop", "root", atomics_core::hook::HookThread::Main),
            (Some("root"), "PermissionRequest", "root", atomics_core::hook::HookThread::Spawned),
            (Some("root"), "SessionStart", "different", atomics_core::hook::HookThread::Main),
        ] {
            state.on_hook(HookMessage {
                pane: pane.map(str::to_owned),
                ancestors: vec![],
                approval_reply: None,
                delivered: 0,
                event: atomics_core::hook::HookEvent {
                    name: event.into(),
                    session_id: id.into(),
                    prompt: Some("foreign prompt".into()),
                    last_assistant_message: Some("foreign reply".into()),
                    thread,
                    ..Default::default()
                },
            });
            assert_eq!(state.sessions[0].status, AgentStatus::Idle, "{pane:?} {event}");
            assert_eq!(state.sessions[0].claude_id.as_deref(), Some("root"));
            assert!(state.sessions[0].approvals.is_empty());
            assert!(state.store.chief_history.is_empty());
        }
        state.config.approvals.allow = vec!["cargo test".into()];
        let reply = Arc::new(ipc::ApprovalReply::default());
        let event = atomics_core::hook::HookEvent::from_payload(&json!({
            "hook_event_name":"PermissionRequest", "session_id":"root",
            "tool_name":"Bash", "tool_input":{"command":"cargo test"}, "cwd":root,
        }))
        .unwrap();
        assert!(state.config.approvals.allows(&ApprovalRequest::from_event(&event), &root));
        state.on_hook(HookMessage {
            pane: Some("root".into()),
            ancestors: vec![],
            delivered: 0,
            approval_reply: Some(reply.clone()),
            event,
        });
        assert_eq!(state.sessions[0].approvals.len(), 1);
        assert!(reply.wait(Duration::ZERO).stdout.is_none());
    });
}

#[test]
fn chief_handoff_recleans_turns_and_closed_pane_values() {
    with_app("app::chief_provider_tests::chief_handoff_recleans_turns_and_closed_pane_values", |app, _| {
        let state = &mut *app.runtime;
        let root = state.launch_dir.clone();
        let mut chief = session("root", AgentKind::Generic, AgentStatus::Idle, &root);
        chief.chief = true;
        state.sessions.push(chief);
        state.sessions.push(session("worker", AgentKind::Claude, AgentStatus::Idle, &root));
        let value = "00000000-0000-4000-8000-000000000001";
        state.record_chief_turn(
            "root",
            &atomics_core::hook::HookEvent {
                name: "Stop".into(),
                session_id: "root".into(),
                last_assistant_message: Some(format!("Completed review {value}")),
                ..Default::default()
            },
            &Default::default(),
        );
        let dir = paths::atomics_home().join("notebook-auth");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("worker"), value).unwrap();
        state.plans.push(Plan { id: "demo".into(), goal: format!("Review {value}"), ..Default::default() });
        state.close_session("worker", false);
        let handoff = state.chief_handoff(Some("root"), ChiefAgent::Codex);
        assert!(handoff.contains("Completed review") && handoff.contains("demo"));
        assert!(!handoff.contains(value), "{handoff}");
    });
}

#[test]
fn chief_empty_codex_command_is_a_configuration_error() {
    with_app("app::chief_provider_tests::chief_empty_codex_command_is_a_configuration_error", |app, _| {
        app.runtime.config.chief = toml::from_str("agent='codex'\ncommand=[]").unwrap();
        assert!(app.runtime.start_chief(None).unwrap_err().contains("command"));
        for arg in [
            "--dangerously-bypass-approvals-and-sandbox",
            "--sandbox=workspace-write",
            "-c=approval_policy='never'",
            "--profile=permissive",
        ] {
            app.runtime.config.chief.command = vec!["codex".into(), arg.into()];
            assert!(app.runtime.start_chief(None).unwrap_err().contains("chief options"));
            app.runtime.config.chief.command.pop();
            app.runtime.config.chief.args = vec![arg.into()];
            assert!(app.runtime.start_chief(None).unwrap_err().contains("chief options"));
            app.runtime.config.chief.args.clear();
        }
    });
}

#[test]
fn chief_local_turn_is_in_the_trace_when_ticket_credentials_are_unavailable() {
    use atomics_core::hook::HookEvent;
    with_app(
        "app::chief_provider_tests::chief_local_turn_is_in_the_trace_when_ticket_credentials_are_unavailable",
        |app, _| {
            let state = &mut *app.runtime;
            let root = state.launch_dir.clone();
            let mut chief = session("root", AgentKind::Generic, AgentStatus::Idle, &root);
            chief.chief = true;
            state.sessions.push(chief);
            state.config.tickets = toml::from_str("system = 'echo'").unwrap();
            state.config.tickets.server = crate::mcp_client::FakeOAuthServer::start().server();
            state.config.tickets.remote.enabled = true;
            state.apply_ticket_config();
            for (name, text) in [("UserPromptSubmit", "Review the code"), ("Stop", "Review complete")] {
                state.capture_chief_trace(
                    0,
                    &HookEvent {
                        name: name.into(),
                        session_id: "root".into(),
                        prompt: Some(text.into()),
                        last_assistant_message: Some(text.into()),
                        ..Default::default()
                    },
                );
            }
            let jobs = state.tickets.trace_jobs.keys().copied().collect::<Vec<_>>();
            assert_eq!(jobs.len(), 2);
            for seq in jobs {
                state.finish_trace(seq, Err("test credential worker unavailable".into()));
            }
            assert_eq!(state.store.chief_history[0].turns.len(), 2);
            let trace = state.trace_data().entries;
            assert!(trace.iter().any(|entry| entry.summary == "Review the code"));
            assert!(trace.iter().any(|entry| entry.summary == "Review complete"));
        },
    );
}
