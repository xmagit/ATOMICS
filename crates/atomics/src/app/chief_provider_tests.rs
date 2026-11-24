use super::tests::{persist_config, session, wait_started, with_app};
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
import json, os, sys, time, tty
if "--version" in sys.argv:
    print("codex-cli 0.169.0")
    sys.exit()
with open(os.path.basename(sys.argv[0]) + "-launch.json", "w") as f:
    json.dump(sys.argv[1:], f)
if "gpt-6.2-rejected" in sys.argv:
    print("model rejected by test provider", flush=True)
    sys.exit(1)
tty.setcbreak(sys.stdin.fileno())
print("›" if os.path.basename(sys.argv[0]) == "codex" else "❯", flush=True)
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
            "--sandbox",
            "read-only",
            "developer_instructions=",
            "hooks.PermissionRequest",
            "hooks.UserPromptSubmit",
            "hooks.Stop",
            "features.shell_snapshot=false",
            "gpt-6.2-sol",
            "default_tools_approval_mode=\"prompt\"",
            "tools.atomics_plan_set.approval_mode=\"approve\"",
        ] {
            assert!(joined.contains(expected), "missing {expected}");
        }
        assert!(!joined.contains("--last"));
        assert!(!joined.contains("ATOMICS_NOTE_CAPABILITY ="));
        for invalid in ["--flag", "sk-proj-fixture-private-token"] {
            assert_eq!(
                state.set_chief_model(ChiefAgent::Codex, invalid, Some(&initial), true).unwrap_err(),
                "This model is unavailable for the selected provider"
            );
        }
        assert!(state.set_chief_model(ChiefAgent::Codex, "gpt-6.2-astra", Some(&initial), false).is_err());
        let hook = |pane: &str, event: &str, text: &str| HookMessage {
            pane: Some(pane.into()),
            ancestors: vec![],
            approval_reply: None,
            delivered: Vec::new(),
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
        let resumed = state.set_chief_model(ChiefAgent::Codex, "gpt-6.2-custom", Some(&initial), false).unwrap();
        wait_started(state, &resumed);
        let args = read("codex");
        assert!(args.windows(2).any(|w| w == ["resume", "root-thread"]));
        assert!(args.windows(2).any(|w| w == ["--model", "gpt-6.2-custom"]));
        assert_eq!(state.chief_session_id(), Some(initial.as_str()));
        state.on_hook(hook(&resumed, "SessionStart", ""));
        tick_until(state, |state| state.chief_session_id() == Some(resumed.as_str()));
        state.on_hook(hook(&initial, "UserPromptSubmit", "late old process"));
        assert_eq!(state.sessions.len(), 1);
        // Valid notebook IDs can be all digits and fall outside generic credential heuristics.
        let capability = "00000000-0000-4000-8000-000000000001";
        std::fs::write(paths::atomics_home().join("notebook-auth").join(&resumed), capability).unwrap();
        state.plans.push(Plan { id: "demo".into(), goal: format!("Review {capability}"), ..Default::default() });
        assert!(!state.chief_handoff(Some("root-thread"), ChiefAgent::Codex).0.contains(capability));
        state.config.chief = toml::from_str("agent='claude'\nmodel='sonnet'").unwrap();
        state.config.chief.command = vec![fake.path().join("claude").to_string_lossy().into_owned()];
        assert_eq!(state.available_chiefs().selected.agent, ChiefAgent::Codex);
        assert_eq!(state.available_chiefs().selected.model.as_deref(), Some("gpt-6.2-custom"));
        assert!(
            matches!(state.chief_author(), atomics_core::tickets::Author::Chief(label) if label.contains("Custom"))
        );
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
        tick_until(state, |state| state.sessions.iter().any(|s| s.id == next && s.pending.is_some()));
        let chief = state.sessions.iter().find(|s| s.id == next).unwrap();
        let handoff = chief.pending.as_ref().unwrap().text.clone();
        assert!(handoff.contains("ATOMICS data") && handoff.contains("The demo is ready."));
        assert!(handoff.contains("Final result after replacement spawn"));
        assert!(handoff.contains("demo") && !handoff.contains(capability));
        assert!(!handoff.contains("late old process"));
        assert_eq!(state.chief_session_id(), Some(resumed.as_str()));
        assert_eq!(state.store.chief_selection.as_ref().unwrap().1.agent, ChiefAgent::Codex);
        // Claude's hook wraps a multiline paste in a matched pair of tags.
        let submitted_text = format!("\n\n<pasted_content id=\"a4b2\">\n{handoff}\n</pasted_content id=\"a4b2\">\n");
        let mut submitted = hook(&next, "UserPromptSubmit", &submitted_text);
        submitted.event.session_id = claude_id.clone();
        state.on_hook(submitted);
        let mut acknowledged = hook(&next, "Stop", "Received.");
        acknowledged.event.session_id = claude_id;
        state.on_hook(acknowledged);
        tick_until(state, |state| state.chief_session_id() == Some(next.as_str()));
        let history = serde_json::to_string(&state.store.chief_history).unwrap();
        assert!(history.contains("Review the demo.") && history.contains("The demo is ready."));
        let (return_handoff, _) = state.chief_handoff(state.sessions[0].claude_id.as_deref(), ChiefAgent::Claude);
        assert!(return_handoff.contains("Review the demo."));
        assert!(return_handoff.contains("Final result after replacement spawn"));
        assert_eq!(return_handoff.matches(super::chief_provider::HANDOFF_PREAMBLE).count(), 1);
        assert!(state.set_chief_model(ChiefAgent::Codex, "gpt-6.2-sol", Some(&resumed), true).is_err());

        let retired =
            state.store.sessions.iter().find(|record| record.chief && record.agent == "codex").unwrap().clone();
        state.repos.push(root.clone());
        assert!(atomics_ui::Backend::recent_sessions(&state.backend()).iter().all(|row| !row.record.chief));
        state.resume_record(retired);
        assert_eq!(state.sessions.iter().filter(|s| s.chief).count(), 1);
        let (handoff, _) = state.chief_handoff(Some("root-thread"), ChiefAgent::Codex);
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
                "#!/bin/sh\ncase \"$1\" in\nauth) echo fixture-private-account; echo '└  10 credentials';;\nmodels) echo demo/local; echo config/credentials; echo /private/credentials.json; echo demo/credential-file >&2;;\nesac\n",
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
                    assert!(row.models.is_empty());
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
            state.usage.codex_chief = atomics_core::usage::Usage::codex(
                &json!({
                    "type":"event_msg", "timestamp":"2026-01-10T00:00:00Z",
                    "payload":{"type":"token_count","rate_limits":{
                        "limit_id":"codex-sol-temporary-bucket", "normal_model_slug":"gpt-6.2-sol",
                        "limit_name":"/private/quota.json", "weekly":null,
                        "secondary":{"window_minutes":10080,"used_percent":80.0,"resets_at":i64::MAX}
                    }}
                })
                .to_string(),
            )
            .unwrap();
            state.usage.codex_chief.plan = Some("/private/credentials.json".into());
            state.usage.codex_chief.session =
                Some(atomics_core::usage::Window { left_percent: 0., resets_at: i64::MAX });
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
            state.usage.codex_chief.session.as_mut().unwrap().resets_at = 1;
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
                delivered: Vec::new(),
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
            delivered: Vec::new(),
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
            ChiefAgent::Codex,
            &atomics_core::hook::HookEvent {
                name: "Stop".into(),
                session_id: "root".into(),
                last_assistant_message: Some(format!("Completed review {value}")),
                ..Default::default()
            },
            &Default::default(),
            None,
        );
        let dir = paths::atomics_home().join("notebook-auth");
        std::fs::create_dir_all(&dir).unwrap();
        let capability = dir.join("worker");
        std::fs::write(&capability, value).unwrap();
        state.plans.push(Plan { id: "demo".into(), goal: format!("Review {value}"), ..Default::default() });
        state.close_session("worker", false);
        std::fs::remove_file(capability).unwrap();
        let (handoff, _) = state.chief_handoff(Some("root"), ChiefAgent::Codex);
        assert!(handoff.contains("Completed review") && handoff.contains("demo"));
        assert!(!handoff.contains(value), "{handoff}");
        state.chief_masks = Default::default();
        state.store = atomics_core::store::Store::load(&paths::store_file());
        assert!(!state.chief_handoff(Some("root"), ChiefAgent::Codex).0.contains(value));
    });
}

#[test]
fn chief_masks_are_applied_to_projected_plan_goals() {
    with_app("app::chief_provider_tests::chief_masks_are_applied_to_projected_plan_goals", |app, _| {
        let state = &mut *app.runtime;
        let value = "00000000-0000-4000-8000-000000000001";
        let root = state.launch_dir.clone();
        state.sessions.push(session("worker", AgentKind::Claude, AgentStatus::Idle, &root));
        let dir = paths::atomics_home().join("notebook-auth");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("worker"), value).unwrap();
        state.plans.push(Plan { id: "demo".into(), goal: format!("Review {value}"), ..Default::default() });
        let projected = serde_json::to_string(&state.plan_infos()).unwrap();
        assert!(projected.contains("Review ***") && !projected.contains(value));
    });
}

#[test]
fn chief_empty_codex_command_is_a_configuration_error() {
    with_app("app::chief_provider_tests::chief_empty_codex_command_is_a_configuration_error", |app, _| {
        app.runtime.config.chief = toml::from_str("agent='codex'\ncommand=[]").unwrap();
        assert!(app.runtime.start_chief(None).unwrap_err().contains("command"));
        for arg in [
            "--dangerously-bypass-approvals-and-sandbox",
            "--yolo",
            "--sandbox=workspace-write",
            "-c=approval_policy='never'",
            "--profile=permissive",
        ] {
            app.runtime.config.chief.command = vec!["codex".into(), arg.into()];
            assert!(app.runtime.start_chief(None).unwrap_err().contains("chief options"));
            app.runtime.config.chief.command.pop();
            if arg == "--dangerously-bypass-approvals-and-sandbox" {
                continue;
            }
            app.runtime.config.chief.args = vec![arg.into()];
            assert!(app.runtime.start_chief(None).unwrap_err().contains("chief options"));
            app.runtime.config.chief.args.clear();
        }
    });
}

#[test]
fn chief_local_turn_waits_when_ticket_credentials_are_unavailable() {
    use atomics_core::hook::HookEvent;
    with_app("app::chief_provider_tests::chief_local_turn_waits_when_ticket_credentials_are_unavailable", |app, _| {
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
                None,
            );
        }
        let jobs = state.tickets.trace_jobs.keys().copied().collect::<Vec<_>>();
        assert_eq!(jobs.len(), 2);
        for seq in jobs {
            state.finish_trace(seq, Err("test credential worker unavailable".into()));
        }
        assert_eq!(state.store.chief_history.len(), 1, "keep the redacted turns across a crash");
        let saved = atomics_core::store::Store::load(&paths::store_file());
        assert_eq!(saved.chief_history[0].turns.len(), 2);
        let history = serde_json::to_string(&saved.chief_history).unwrap();
        assert!(!history.contains("Review the code") && !history.contains("Review complete"));
    });
}

#[test]
fn chief_shipped_claude_model_is_recognized() {
    with_app("app::chief_provider_tests::chief_shipped_claude_model_is_recognized", |app, _| {
        let state = &mut *app.runtime;
        state.config.chief = Default::default();
        assert_eq!(state.chief_selection().model.as_deref(), Some("claude-opus-5-5"));
        let options = state.available_chiefs();
        assert_eq!(options.selected.model.as_deref(), Some("claude-opus-5-5"));
        assert!(options.providers[0].models.contains(&"claude-opus-5-5".into()));
        assert_eq!(atomics_core::tickets::pretty_model("claude-opus-5-5"), "Claude Opus 5.5");
    });
}

#[track_caller]
fn tick_until(state: &mut State, condition: impl Fn(&State) -> bool) {
    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
    while !condition(state) {
        assert!(Instant::now() < deadline, "chief fixture timed out: {:?}", state.toast);
        state.runtime_tick();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The PTY leader and its descendants send real socket requests. Scheduling them by files lets the
/// tests enqueue several hooks before allowing the host to process any of them.
fn install_socket_chief(state: &mut State, agent: ChiefAgent, models: &[&str]) {
    let programs = state.launch_dir.join("providers");
    std::fs::create_dir_all(&programs).unwrap();
    let program = programs.join(agent.name());
    crate::test_support::write_program(
        &program,
        r#"#!/usr/bin/env python3
import json, os, pathlib, socket, subprocess, sys, threading, time, tty
if "--version" in sys.argv:
    print("codex-cli 0.169.0")
    sys.exit()
argv = sys.argv[1:]
pane = os.environ.get("ATOMICS_HOOK_PANE")
if not pane:
    mcp = json.loads(argv[argv.index("--mcp-config") + 1])
    pane = mcp["mcpServers"]["atomics"]["env"]["ATOMICS_PANE"]
key = os.environ["ATOMICS_CHIEF_HOOK_BINDING"]
root = argv[argv.index("--session-id") + 1] if "--session-id" in argv else "codex-root"

def forward(path, request):
    with socket.socket(socket.AF_UNIX) as sock:
        sock.connect(os.environ["ATOMICS_SOCKET"])
        sock.sendall((json.dumps(request) + "\n").encode())
        result = sock.makefile().readline()
    writing = pathlib.Path(str(path) + ".writing")
    writing.write_text(result)
    writing.rename(path)

launch = pathlib.Path(pane + ".launch-writing")
launch.write_text(json.dumps({
    "root": root, "args": argv, "home": os.environ.get("CODEX_HOME", ""), "tmp": os.environ.get("TMPDIR", "")
}))
launch.rename(pane + ".launch")
tty.setcbreak(sys.stdin.fileno())
print("❯" if "--session-id" in argv or "--resume" in argv else "›", flush=True)
while True:
    for path in pathlib.Path(".").glob(pane + ".send-*"):
        if path.name.endswith(".writing"):
            continue
        request = json.loads(path.read_text())
        slot = pathlib.Path(path.name.replace(".send-", ".recv-"))
        path.unlink()
        if request.pop("exit", False):
            sys.exit(7)
        if "screen" in request:
            print(request["screen"], flush=True)
            slot.write_text("printed")
            if request.get("listen"):
                pathlib.Path(str(slot) + ".key").write_bytes(os.read(sys.stdin.fileno(), 1))
            continue
        origin = request.pop("origin", "root")
        if "native_command" in request:
            command = request["native_command"]
            project = json.dumps(str(pathlib.Path.cwd())) + ' = { trust_level = "untrusted" }'
            dirs = [pathlib.Path(os.environ["CODEX_HOME"])]
            if not any(arg.startswith("projects=") and project in arg for arg in argv):
                dirs.append(pathlib.Path(".codex"))
            pattern = json.dumps(command.split())
            if any(pattern in f.read_text() for folder in dirs for f in folder.glob("rules/*.rules")):
                slot.write_text('"native grant"')
                continue
            request = {"hook_event_name": "PermissionRequest", "tool_name": "Bash", "tool_input": {"command": command}}
        if "hook_event_name" in request:
            request.setdefault("session_id", root)
            request = {"pane": pane, "chief_key": key, "payload": request}
        if origin == "child":
            subprocess.run([sys.executable, "-c",
                "import os, socket, sys; s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.sendall(sys.argv[2].encode()); "
                "open(sys.argv[3] + '.writing', 'w').write(s.makefile().readline()); os.rename(sys.argv[3] + '.writing', sys.argv[3])",
                os.environ["ATOMICS_SOCKET"], json.dumps(request) + "\n", str(slot)], check=True)
        else:
            threading.Thread(target=forward, args=(slot, request), daemon=True).start()
    time.sleep(0.005)
"#,
    );
    state.config.agents.iter_mut().find(|a| a.name == agent.name()).unwrap().command =
        vec![program.to_string_lossy().into_owned()];
    state.chief_probe_at = Some(Instant::now());
    state.chief_providers.push(ChiefProvider {
        agent,
        installed: true,
        auth: ChiefAuth::SignedIn,
        models: models.iter().map(|model| (*model).into()).collect(),
        ..Default::default()
    });
}

fn fixture_read(path: &Path) -> String {
    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            return text;
        }
        assert!(Instant::now() < deadline, "fixture did not write {}", path.display());
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn fixture_launch(state: &mut State, pane: &str) -> serde_json::Value {
    wait_started(state, pane);
    let path = state.launch_dir.join(format!("{pane}.launch"));
    serde_json::from_str(&fixture_read(&path)).unwrap()
}

fn fixture_send(state: &State, pane: &str, name: &str, payload: serde_json::Value) -> PathBuf {
    let path = state.launch_dir.join(format!("{pane}.send-{name}"));
    atomics_core::store::write_private(&path, payload.to_string().as_bytes()).unwrap();
    state.launch_dir.join(format!("{pane}.recv-{name}"))
}

#[test]
fn chief_socket_checks_root_origin_and_accepts_verified_conversation_transitions() {
    with_app(
        "app::chief_provider_tests::chief_socket_checks_root_origin_and_accepts_verified_conversation_transitions",
        |app, _| {
            let state = &mut *app.runtime;
            install_socket_chief(state, ChiefAgent::Codex, &["gpt-6.2-sol"]);
            state.config.chief = toml::from_str("agent='codex'").unwrap();
            state.config.chief.cwd = Some(state.launch_dir.to_string_lossy().into_owned());
            let chief = state.start_chief(None).unwrap();
            fixture_launch(state, &chief);
            fixture_read(&fixture_send(state, &chief, "start", json!({"hook_event_name":"SessionStart"})));
            let bad = fixture_send(
                state,
                &chief,
                "foreign",
                json!({
                    "origin":"child", "hook_event_name":"UserPromptSubmit", "prompt":"Unverified child prompt"
                }),
            );
            assert_eq!(fixture_read(&bad), "");
            for (source, conversation) in [("clear", "cleared"), ("resume", "resumed")] {
                fixture_read(&fixture_send(
                    state,
                    &chief,
                    source,
                    json!({
                        "hook_event_name":"SessionStart", "source":source, "session_id":conversation
                    }),
                ));
                fixture_read(&fixture_send(
                    state,
                    &chief,
                    "prompt",
                    json!({
                        "hook_event_name":"UserPromptSubmit", "session_id":conversation, "prompt":format!("{source} prompt")
                    }),
                ));
                fixture_read(&fixture_send(
                    state,
                    &chief,
                    "reply",
                    json!({
                        "hook_event_name":"Stop", "session_id":conversation, "last_assistant_message":format!("{source} reply")
                    }),
                ));
                std::fs::remove_file(state.launch_dir.join(format!("{chief}.recv-prompt"))).unwrap();
                std::fs::remove_file(state.launch_dir.join(format!("{chief}.recv-reply"))).unwrap();
            }
            state.runtime_tick();
            assert_eq!(state.sessions[0].claude_id.as_deref(), Some("resumed"));
            assert_eq!(state.store.chief_history.len(), 2);
            assert!(state.store.chief_history.iter().all(|conversation| conversation.turns.len() == 2));
            crate::app::tests::settle_trace(state);
            assert_eq!(state.trace_data().entries.len(), 2);
        },
    );
}

pub(super) fn start_switch_fixture(state: &mut State) -> String {
    install_socket_chief(state, ChiefAgent::Codex, &["gpt-6.2-sol", "gpt-6.2-astra"]);
    install_socket_chief(state, ChiefAgent::Claude, &["sonnet"]);
    state.config.chief = toml::from_str("agent='codex'").unwrap();
    state.config.chief.cwd = Some(state.launch_dir.to_string_lossy().into_owned());
    let chief = state.start_chief(None).unwrap();
    fixture_launch(state, &chief);
    fixture_read(&fixture_send(state, &chief, "start", json!({"hook_event_name":"SessionStart"})));
    state.runtime_tick();
    chief
}

#[test]
fn chief_switch_only_allows_the_active_launch_to_answer_approvals() {
    with_app("app::chief_provider_tests::chief_switch_only_allows_the_active_launch_to_answer_approvals", |app, _| {
        let state = &mut *app.runtime;
        let chief = start_switch_fixture(state);
        let candidate = state.set_chief_model(ChiefAgent::Claude, "sonnet", Some(&chief), false).unwrap();
        fixture_launch(state, &candidate);
        assert_eq!(state.describe(state.find(&chief).unwrap())["chief"], true);
        let preparing = state.describe(state.find(&candidate).unwrap());
        assert_eq!(preparing["chief"], false);
        assert_eq!(preparing["status"], "preparing");
        let root = state.launch_dir.clone();
        state.sessions.push(session("worker", AgentKind::Generic, AgentStatus::Idle, &root));
        let hook_reply = Arc::new(ipc::ApprovalReply::default());
        state.on_hook(HookMessage {
            pane: Some("worker".into()),
            ancestors: Vec::new(),
            delivered: Vec::new(),
            approval_reply: Some(Arc::clone(&hook_reply)),
            event: atomics_core::hook::HookEvent::from_payload(&json!({
                "hook_event_name":"PermissionRequest", "session_id":"worker",
                "tool_name":"Bash", "tool_input":{"command":"printf hello"}, "cwd":root
            }))
            .unwrap(),
        });
        let worker = state.find("worker").unwrap();
        let approval = state.sessions[worker].approvals[0].id.clone();
        let digest = state.approval_json(worker, &state.sessions[worker].approvals[0], 0, true)["digest"].clone();
        let attempt = |pane: &str| {
            json!({
                "chief_scope":pane, "cmd":"approve", "session":"worker", "request":approval, "digest":digest, "decision":"deny"
            })
        };
        let refused = fixture_send(state, &candidate, "decision", attempt(&candidate));
        tick_until(state, |_| refused.is_file());
        let refused: serde_json::Value = serde_json::from_str(&fixture_read(&refused)).unwrap();
        assert_eq!(refused["error"], "This chief has not been promoted");
        assert_eq!(state.sessions[worker].approvals[0].id, approval);
        let allowed = fixture_send(state, &chief, "decision", attempt(&chief));
        tick_until(state, |_| allowed.is_file());
        let allowed: serde_json::Value = serde_json::from_str(&fixture_read(&allowed)).unwrap();
        assert_eq!(allowed["ok"], true, "{allowed}");
        assert_eq!(hook_reply.wait(Duration::ZERO).stdout, Some(Decision::Deny.output(None)));
    });
}

#[test]
fn chief_switch_rolls_back_when_the_candidate_exits_before_acknowledging_the_handoff() {
    with_app(
        "app::chief_provider_tests::chief_switch_rolls_back_when_the_candidate_exits_before_acknowledging_the_handoff",
        |app, _| {
            let state = &mut *app.runtime;
            let chief = start_switch_fixture(state);
            let candidate = state.set_chief_model(ChiefAgent::Claude, "sonnet", Some(&chief), false).unwrap();
            fixture_launch(state, &candidate);
            fixture_read(&fixture_send(state, &candidate, "start", json!({"hook_event_name":"SessionStart"})));
            tick_until(state, |state| {
                !state.available_chiefs().switching
                    || state.sessions.iter().any(|s| s.id == candidate && s.pending.is_some())
            });
            assert_eq!(state.chief_session_id(), Some(chief.as_str()));
            assert!(state.store.chief_selection.is_none());
            let handoff = state.sessions[state.find(&candidate).unwrap()].pending.as_ref().unwrap().text.clone();
            fixture_read(&fixture_send(
                state,
                &candidate,
                "submitted",
                json!({"hook_event_name":"UserPromptSubmit","prompt":handoff}),
            ));
            state.runtime_tick();
            assert_eq!(state.chief_session_id(), Some(chief.as_str()));
            assert!(state.sessions[state.find(&candidate).unwrap()].pending.is_none());
            fixture_send(state, &candidate, "exit", json!({"exit":true}));
            tick_until(state, |state| !state.available_chiefs().switching);
            assert_eq!(state.chief_session_id(), Some(chief.as_str()));
            assert!(atomics_core::store::Store::load(&paths::store_file()).chief_selection.is_none());
        },
    );
}

#[test]
fn chief_switch_cancels_when_the_candidate_requests_permission() {
    with_app("app::chief_provider_tests::chief_switch_cancels_when_the_candidate_requests_permission", |app, _| {
        let state = &mut *app.runtime;
        let chief = start_switch_fixture(state);
        let rejected = state.set_chief_model(ChiefAgent::Claude, "sonnet", Some(&chief), false).unwrap();
        fixture_launch(state, &rejected);
        fixture_read(&fixture_send(state, &rejected, "start", json!({"hook_event_name":"SessionStart"})));
        fixture_send(
            state,
            &rejected,
            "permission",
            json!({
                "hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"printf hello"}
            }),
        );
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
        while state.hub.lock().unwrap().queued_chief_hooks(&rejected) != 2 {
            assert!(Instant::now() < deadline, "both startup and permission must reach the socket before the tick");
            std::thread::sleep(Duration::from_millis(5));
        }
        tick_until(state, |state| !state.available_chiefs().switching);
        assert_eq!(state.chief_session_id(), Some(chief.as_str()));
        assert!(state.toast.as_ref().unwrap().0.contains("asked for permission"));
    });
}

#[test]
fn chief_switch_restarts_a_same_provider_candidate_after_an_old_turn() {
    with_app(
        "app::chief_provider_tests::chief_switch_restarts_a_same_provider_candidate_after_an_old_turn",
        |app, _| {
            let state = &mut *app.runtime;
            let chief = start_switch_fixture(state);
            let stale = state.set_chief_model(ChiefAgent::Codex, "gpt-6.2-astra", Some(&chief), false).unwrap();
            fixture_launch(state, &stale);
            fixture_read(&fixture_send(state, &stale, "start", json!({"hook_event_name":"SessionStart"})));
            fixture_read(&fixture_send(
                state,
                &chief,
                "prompt",
                json!({"hook_event_name":"UserPromptSubmit","prompt":"One more turn"}),
            ));
            fixture_read(&fixture_send(
                state,
                &chief,
                "reply",
                json!({"hook_event_name":"Stop","last_assistant_message":"Completed after the candidate started"}),
            ));
            state.runtime_tick();
            assert!(state.find(&stale).is_none());
            assert_eq!(state.chief_session_id(), Some(chief.as_str()));
            let replacement = state.sessions.iter().find(|s| state.is_chief_replacement(&s.id)).unwrap().id.clone();
            let launch = fixture_launch(state, &replacement);
            assert!(launch["args"].as_array().unwrap().windows(2).any(|pair| pair == ["resume", "codex-root"]));
            fixture_read(&fixture_send(state, &replacement, "start", json!({"hook_event_name":"SessionStart"})));
            tick_until(state, |state| state.chief_session_id() == Some(replacement.as_str()));
        },
    );
}

#[test]
fn chief_codex_does_not_inherit_native_command_grants() {
    with_app("app::chief_provider_tests::chief_codex_does_not_inherit_native_command_grants", |app, _| {
        let state = &mut *app.runtime;
        install_socket_chief(state, ChiefAgent::Codex, &["gpt-6.2-sol"]);
        state.config.chief = toml::from_str("agent='codex'").unwrap();
        state.config.chief.cwd = Some(state.launch_dir.to_string_lossy().into_owned());
        let ordinary = crate::setup::instance_agent_env()
            .into_iter()
            .find(|(name, _)| name == "CODEX_HOME")
            .map(|(_, home)| PathBuf::from(home))
            .unwrap();
        let user_home = crate::setup::user_codex_home();
        let auth = b"{\"fixture\":\"credential file\"}";
        atomics_core::store::write_private(&user_home.join("auth.json"), auth).unwrap();
        atomics_core::store::write_private(&user_home.join("config.toml"), b"fixture = 'user config'").unwrap();
        let state_dir = crate::setup::chief_codex_home();
        let legacy = state_dir.join("launch-legacy");
        atomics_core::store::write_private(&legacy.join("auth.json"), auth).unwrap();
        let unrelated = state_dir.join("sessions/auth.json");
        atomics_core::store::write_private(&unrelated, b"leave unrelated files alone").unwrap();
        let command = "printf harmless";
        for dir in [ordinary.clone(), user_home.clone(), state.launch_dir.join(".codex")] {
            let path = dir.join("rules/default.rules");
            atomics_core::store::write_private(
                &path,
                b"prefix_rule(pattern=[\"printf\", \"harmless\"], decision=\"allow\")\n",
            )
            .unwrap();
        }
        let chief = state.start_chief(None).unwrap();
        let launch = fixture_launch(state, &chief);
        fixture_read(&fixture_send(state, &chief, "start", json!({"hook_event_name":"SessionStart"})));
        let reply = fixture_send(state, &chief, "native-test", json!({"native_command":command}));
        tick_until(state, |state| !state.sessions[0].approvals.is_empty() || reply.is_file());
        assert!(!reply.is_file(), "the chief inherited a native grant");
        assert_eq!(state.sessions[0].approvals[0].request.command.as_deref(), Some(command));
        let home = PathBuf::from(launch["home"].as_str().unwrap());
        assert_ne!(home, ordinary);
        assert!(!home.join("rules/default.rules").is_file());
        let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(config.contains("trusted_hash") && config.contains("permission_request"));
        assert!(!config.contains("user config"));
        assert!(!legacy.join("auth.json").exists(), "remove copies left by older launches");
        assert!(unrelated.exists());
        assert!(!home.starts_with(&state_dir));
        let tmp = PathBuf::from(launch["tmp"].as_str().unwrap());
        assert!(tmp.is_dir() && tmp.starts_with(&home) && tmp != home);
        assert_eq!(std::fs::read_link(home.join("auth.json")).unwrap(), user_home.join("auth.json"));
        assert_eq!(std::fs::read(home.join("auth.json")).unwrap(), auth);
        atomics_core::store::write_private(&home.join("rules/default.rules"), b"local grant").unwrap();
        state.close_session(&chief, false);
        let chief = state.start_chief(None).unwrap();
        let launch = fixture_launch(state, &chief);
        let renewed = PathBuf::from(launch["home"].as_str().unwrap());
        assert!(!renewed.join("rules/default.rules").exists());
        fixture_send(state, &chief, "exit", json!({"exit":true}));
        tick_until(state, |state| state.sessions[0].status == AgentStatus::Exited);
        assert!(!renewed.exists(), "the launch home must be removed on provider exit");
    });
}

#[test]
fn chief_initial_prompts_are_masked_before_launch() {
    with_app("app::chief_provider_tests::chief_initial_prompts_are_masked_before_launch", |app, _| {
        let state = &mut *app.runtime;
        let secret = "1234567890-registered-fixture-value";
        state.remember_chief_masks(&[secret.into()]);
        install_socket_chief(state, ChiefAgent::Claude, &["haiku"]);
        install_socket_chief(state, ChiefAgent::Codex, &["gpt-6.2-sol"]);
        state.config.chief.command = state.config.agent("claude").unwrap().command.clone();
        state.config.chief.cwd = Some(state.launch_dir.to_string_lossy().into_owned());
        let claude = state.start_chief(Some(format!("Claude marker {secret}"))).unwrap();
        let launch = fixture_launch(state, &claude);
        let mut violations = Vec::new();
        if launch.to_string().contains(secret) {
            violations.push("Claude launch args");
        }
        atomics_core::store::flush_writes();
        if std::fs::read_to_string(paths::store_file()).unwrap().contains(secret) {
            violations.push("session records");
        }
        state.close_session(&claude, false);
        state.config.chief.agent = ChiefAgent::Codex;
        state.config.chief.command = state.config.agent("codex").unwrap().command.clone();
        let prompt = format!("Codex marker {secret} {}", "long prompt ".repeat(2500));
        let codex = state.start_chief(Some(prompt)).unwrap();
        fixture_launch(state, &codex);
        let brief = std::fs::read_to_string(paths::briefs_dir().join(format!("{codex}.md"))).unwrap();
        assert!(brief.contains("Codex marker"));
        if brief.contains(secret) {
            violations.push("Codex brief");
        }
        assert!(violations.is_empty(), "{violations:?}");
    });
}

#[test]
fn chief_composed_instructions_are_masked_for_both_providers() {
    with_app("app::chief_provider_tests::chief_composed_instructions_are_masked_for_both_providers", |app, _| {
        let state = &mut *app.runtime;
        let secret = "1234567890-instruction-fixture-value";
        state.remember_chief_masks(&[secret.into()]);
        std::fs::write(&state.config_path, format!("[chief]\nprompt = 'Config marker {secret}'\n")).unwrap();
        std::fs::write(state.launch_dir.join("ATOMICS.md"), format!("Repository marker {secret}")).unwrap();
        state.repos = vec![state.launch_dir.clone()];
        state.open_projects = state.repos.clone();
        state.config.chief.cwd = Some(state.launch_dir.to_string_lossy().into_owned());
        for (provider, model) in [(ChiefAgent::Claude, "haiku"), (ChiefAgent::Codex, "gpt-6.2-sol")] {
            install_socket_chief(state, provider, &[model]);
            state.config.chief.agent = provider;
            state.config.chief.command = state.config.agent(provider.name()).unwrap().command.clone();
            let pane = state.start_chief(None).unwrap();
            let launch = fixture_launch(state, &pane);
            let args = launch["args"].as_array().unwrap();
            let instructions = args
                .iter()
                .filter_map(serde_json::Value::as_str)
                .find(|arg| arg.contains("Config marker") && arg.contains("Repository marker"))
                .expect("both instruction sources reached the provider");
            assert!(!instructions.contains(secret), "{provider:?} received the registered value");
            state.close_session(&pane, false);
        }
    });
}

#[test]
fn chief_promptless_instructions_wait_for_current_masks() {
    with_app("app::chief_provider_tests::chief_promptless_instructions_wait_for_current_masks", |app, _| {
        use crate::mcp_client::{FakeOAuthServer, McpClient};

        let state = &mut *app.runtime;
        let fake = FakeOAuthServer::start();
        fake.state.lock().unwrap().expires_in = 30;
        let server = fake.server();
        let (_dir, oauth) = crate::mcp_client::oauth_test_machine();
        oauth
            .sign_in_via(&server, Duration::from_secs(10), &|url| {
                let url = url.to_string();
                std::thread::spawn(move || crate::mcp_client::oauth_test_browse(&url));
                Ok(())
            })
            .unwrap();
        let old = fake.state.lock().unwrap().access.iter().next().unwrap().clone();
        state.remember_chief_masks(std::slice::from_ref(&old));
        fake.state.lock().unwrap().expires_in = 3600;
        let client = McpClient::connect_with(&server, Duration::from_secs(5), |_| None, Some(&oauth)).unwrap();
        let current = client.oauth_token().unwrap();
        assert!(current != old && !state.chief_masks.secrets().iter().any(|mask| mask == current));
        state.config.tickets = toml::from_str("system = 'echo'").unwrap();
        state.config.tickets.server = server;
        state.tickets.sign.override_oauth(oauth);
        state.config.chief.prompt = format!("Config marker {current}");
        std::fs::write(state.launch_dir.join("ATOMICS.md"), format!("Repository marker {current}")).unwrap();
        state.config.chief.system_prompt = format!("System marker {current}");
        state.repos = vec![state.launch_dir.clone()];
        state.open_projects = state.repos.clone();
        state.config.chief.cwd = Some(state.launch_dir.to_string_lossy().into_owned());
        let prior_masks = state.chief_masks.clone();
        let prior_fingerprints = state.store.chief_mask_fingerprints.clone();
        for (provider, model) in [(ChiefAgent::Claude, "haiku"), (ChiefAgent::Codex, "gpt-6.2-sol")] {
            install_socket_chief(state, provider, &[model]);
            state.config.chief.agent = provider;
            state.config.chief.command = state.config.agent(provider.name()).unwrap().command.clone();
            persist_config(state);
            state.chief_masks = prior_masks.clone();
            state.store.chief_mask_fingerprints = prior_fingerprints.clone();
            assert!(state.chief_cleaner(&state.cleaner()).text(current) == current);
            let pane = state.start_chief(None).unwrap();
            let launch = fixture_launch(state, &pane).to_string();
            assert!(!launch.contains(current), "{provider:?} received the ticket credential at launch");
            assert!(!launch.contains("marker"), "{provider:?} received dynamic instructions at launch");
            assert!(launch.contains("You are the chief agent of ATOMICS"));
            tick_until(state, |state| state.tickets.trace_jobs.is_empty());
            let start = fixture_send(state, &pane, "start", json!({"hook_event_name":"SessionStart"}));
            let reply: atomics_core::hook::HookReply = serde_json::from_str(&fixture_read(&start)).unwrap();
            let context = reply.stdout.unwrap();
            for marker in ["System marker", "Config marker", "Repository marker"] {
                assert!(context.contains(&format!("{marker} ***")), "{provider:?} did not receive masked {marker}");
            }
            assert!(!context.contains(current));
            state.close_session(&pane, false);
        }
    });
}

#[test]
fn chief_promptless_instructions_stay_withheld_when_masks_cannot_be_read() {
    with_app(
        "app::chief_provider_tests::chief_promptless_instructions_stay_withheld_when_masks_cannot_be_read",
        |app, _| {
            let state = &mut *app.runtime;
            let fake = crate::mcp_client::FakeOAuthServer::start();
            let (dir, oauth) = crate::mcp_client::oauth_test_machine();
            std::fs::write(dir.path().join("fail-keychain"), "").unwrap();
            state.config.tickets = toml::from_str("system = 'echo'").unwrap();
            state.config.tickets.server = fake.server();
            state.tickets.sign.override_oauth(oauth);
            state.config.chief.prompt = "Config marker".into();
            std::fs::write(state.launch_dir.join("ATOMICS.md"), "Repository marker").unwrap();
            state.repos = vec![state.launch_dir.clone()];
            state.open_projects = state.repos.clone();
            state.config.chief.cwd = Some(state.launch_dir.to_string_lossy().into_owned());
            install_socket_chief(state, ChiefAgent::Codex, &["gpt-6.2-sol"]);
            state.config.chief.agent = ChiefAgent::Codex;
            state.config.chief.command = state.config.agent("codex").unwrap().command.clone();
            persist_config(state);
            let pane = state.start_chief(None).unwrap();
            let launch = fixture_launch(state, &pane).to_string();
            assert!(!launch.contains("marker"));
            tick_until(state, |state| state.tickets.trace_jobs.is_empty());
            assert!(state.toast.as_ref().is_some_and(|(text, _)| text.contains("instructions are withheld")));
            for event in ["SessionStart", "UserPromptSubmit"] {
                let reply = fixture_read(&fixture_send(state, &pane, event, json!({"hook_event_name":event})));
                assert!(!reply.contains("marker"));
            }
        },
    );
}

#[test]
fn chief_first_send_waits_for_current_masks_and_holds_on_failure() {
    with_app("app::chief_provider_tests::chief_first_send_waits_for_current_masks_and_holds_on_failure", |app, _| {
        let state = &mut *app.runtime;
        let fake = crate::mcp_client::FakeOAuthServer::start();
        let (_dir, oauth) = crate::mcp_client::oauth_test_machine();
        state.config.tickets = toml::from_str("system = 'echo'").unwrap();
        state.config.tickets.server = fake.server();
        state.apply_ticket_config();
        state.tickets.sign.override_oauth(oauth);
        install_socket_chief(state, ChiefAgent::Codex, &["gpt-6.2-sol"]);
        state.config.chief.agent = ChiefAgent::Codex;
        state.config.chief.command = state.config.agent("codex").unwrap().command.clone();
        state.config.chief.cwd = Some(state.launch_dir.to_string_lossy().into_owned());
        let pane = state.start_chief(None).unwrap();
        fixture_launch(state, &pane);
        let index = state.find(&pane).unwrap();
        assert!(!state.tickets.trace_jobs.is_empty(), "startup mask read is outstanding");
        let secret = "1234567890-current-oauth-fixture";
        let text = format!("First send {secret}");
        assert_eq!(state.deliver(index, &text), Delivery::Held);
        let pending = *state.tickets.trace_jobs.keys().max().unwrap();
        state.release_held(index);
        assert!(state.sessions[index].pending.is_none());
        state.finish_trace(pending, Err("keychain temporarily unavailable".into()));
        state.release_held(index);
        assert!(state.sessions[index].pending.is_none());
        assert_eq!(state.sessions[index].held, [text]);
        atomics_core::store::flush_writes();
        assert!(!std::fs::read_to_string(paths::store_file()).unwrap_or_default().contains(secret));

        assert_eq!(state.deliver(index, "Continue"), Delivery::Held);
        let pending = *state.tickets.trace_jobs.keys().max().unwrap();
        state.finish_trace(
            pending,
            Ok(crate::tickets::TicketOutput::TraceCleaner(atomics_core::tickets::Cleaner::new([secret.into()]))),
        );
        state.release_held(index);
        let sent = &state.sessions[index].pending.as_ref().expect("verified message was sent").text;
        assert_eq!(sent, "First send ***\n\nContinue");
    });
}

#[test]
fn chief_candidate_cancels_for_a_screen_permission_menu() {
    with_app("app::chief_provider_tests::chief_candidate_cancels_for_a_screen_permission_menu", |app, _| {
        let state = &mut *app.runtime;
        let chief = start_switch_fixture(state);
        let candidate = state.set_chief_model(ChiefAgent::Claude, "sonnet", Some(&chief), false).unwrap();
        fixture_launch(state, &candidate);
        let output = fixture_send(
            state,
            &candidate,
            "screen",
            json!({
                "screen": "Bash command\n  printf harmless\nDo you want to proceed?\n❯ 1. Yes\n  2. No"
            }),
        );
        fixture_read(&output);
        let index = state.find(&candidate).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !state.sessions[index].terminal.as_ref().unwrap().screen_text(60).contains("printf harmless") {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(state.poll_chief_replacement());
        assert_eq!(state.chief_session_id(), Some(chief.as_str()));
        assert!(state.toast.as_ref().unwrap().0.contains("asked for permission"));
    });
}

#[test]
fn chief_codex_bypass_in_chief_args_omits_default_permission_flags() {
    with_app("app::chief_provider_tests::chief_codex_bypass_in_chief_args_omits_default_permission_flags", |app, _| {
        let state = &mut *app.runtime;
        install_socket_chief(state, ChiefAgent::Codex, &["gpt-6.2-sol"]);
        state.config.chief = toml::from_str(
            "agent='codex'\nargs=['--dangerously-bypass-approvals-and-sandbox', '-c', 'model_reasoning_effort=high']",
        )
        .unwrap();
        state.config.chief.cwd = Some(state.launch_dir.to_string_lossy().into_owned());
        let chief = state.start_chief(None).unwrap();
        let launch = fixture_launch(state, &chief);
        let args = launch["args"].as_array().unwrap();
        assert!(args.iter().any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox"));
        assert!(args.windows(2).any(|pair| pair == ["-c", "model_reasoning_effort=high"]));
        assert!(args.windows(2).any(|pair| pair == ["--model", "gpt-6.2-sol"]));
        assert!(!args.iter().any(|arg| arg == "--sandbox" || arg == "-a"));
    });
}

#[test]
fn codex_chief_standard_tier_launches_with_xhigh_reasoning() {
    with_app("app::chief_provider_tests::codex_chief_standard_tier_launches_with_xhigh_reasoning", |app, _| {
        let state = &mut *app.runtime;
        install_socket_chief(state, ChiefAgent::Codex, &["gpt-6.2-astra"]);
        state.config.chief = toml::from_str(
            "agent='codex'\nmodel='gpt-6.2-astra'\nargs=['-c', 'service_tier=default', '-c', 'model_reasoning_effort=xhigh']",
        )
        .unwrap();
        state.config.chief.cwd = Some(state.launch_dir.to_string_lossy().into_owned());
        let chief = state.start_chief(None).unwrap();
        let launch = fixture_launch(state, &chief);
        let args = launch["args"].as_array().unwrap();
        assert!(args.windows(2).any(|pair| pair == ["--model", "gpt-6.2-astra"]));
        assert!(
            args.windows(4)
                .any(|options| { options == ["-c", "service_tier=default", "-c", "model_reasoning_effort=xhigh"] })
        );
    });
}

#[test]
fn chief_codex_opens_a_restricted_folder_before_its_first_hook() {
    with_app("app::chief_provider_tests::chief_codex_opens_a_restricted_folder_before_its_first_hook", |app, _| {
        let state = &mut *app.runtime;
        install_socket_chief(state, ChiefAgent::Codex, &["gpt-6.2-sol"]);
        state.config.chief = toml::from_str("agent='codex'").unwrap();
        let cwd = state.launch_dir.canonicalize().unwrap();
        state.config.chief.cwd = Some(cwd.to_string_lossy().into_owned());
        let chief = state.start_chief(None).unwrap();
        let launch = fixture_launch(state, &chief);
        let args = launch["args"].as_array().unwrap();
        assert!(args.windows(2).any(|pair| pair == ["--sandbox", "read-only"]));
        assert!(args.windows(2).any(|pair| pair == ["-a", "on-request"]));
        assert!(args.iter().any(|arg| {
            arg.as_str().is_some_and(|arg| {
                arg.starts_with("projects=") && arg.contains(cwd.to_str().unwrap()) && arg.contains("untrusted")
            })
        }));
        let menu =
            format!("Folder access\n{}\n\n› 1. Open restricted\n  2. Quit\n\nenter continue · esc quit", cwd.display());
        let output = fixture_send(state, &chief, "restricted", json!({"screen":menu, "listen":true}));
        fixture_read(&output);
        let key = PathBuf::from(format!("{}.key", output.display()));
        tick_until(state, |_| key.is_file());
        assert!(matches!(fixture_read(&key).as_str(), "\r" | "\n"));
        assert!(state.events.iter().any(|event| {
            event.session == chief && event.kind() == "startup" && event.change == Some("codex_opened_restricted")
        }));
    });
}

#[test]
fn chief_codex_opens_the_folder_in_restricted_mode() {
    with_app("app::chief_provider_tests::chief_codex_opens_the_folder_in_restricted_mode", |app, _| {
        let state = &mut *app.runtime;
        let chief = start_switch_fixture(state);
        let candidate = state.set_chief_model(ChiefAgent::Codex, "gpt-6.2-astra", Some(&chief), false).unwrap();
        fixture_launch(state, &candidate);
        let cwd = state.sessions[state.find(&candidate).unwrap()].cwd.canonicalize().unwrap();
        let menu = format!(
            "Folder access\n{}\n\n› 1. Open restricted\n  2. Back to Agent Command Center\n\nenter continue · esc back",
            cwd.display()
        );
        let output = fixture_send(state, &candidate, "restricted", json!({"screen":menu, "listen":true}));
        fixture_read(&output);
        let key = PathBuf::from(format!("{}.key", output.display()));
        // The generic screen poll can hold input for this menu without finding an approval.
        let index = state.find(&candidate).unwrap();
        state.sessions[index].approval_hold = true;
        tick_until(state, |state| key.is_file() || !state.available_chiefs().switching);
        assert!(state.available_chiefs().switching);
        assert_eq!(state.chief_session_id(), Some(chief.as_str()));
        assert!(matches!(fixture_read(&key).as_str(), "\r" | "\n"));
    });
}

#[test]
fn chief_codex_leaves_an_overlay_above_stale_folder_options_alone() {
    with_app("app::chief_provider_tests::chief_codex_leaves_an_overlay_above_stale_folder_options_alone", |app, _| {
        let state = &mut *app.runtime;
        let chief = start_switch_fixture(state);
        let candidate = state.set_chief_model(ChiefAgent::Codex, "gpt-6.2-astra", Some(&chief), false).unwrap();
        fixture_launch(state, &candidate);
        let cwd = state.sessions[state.find(&candidate).unwrap()].cwd.canonicalize().unwrap();
        let screen = format!(
            "Folder access\n{}\n\n› 1. Open restricted\n  2. Back to Agent Command Center\n\nenter continue · esc back\n\nConfirm this action? [y/N]",
            cwd.display()
        );
        let output = fixture_send(state, &candidate, "overlay", json!({"screen":screen, "listen":true}));
        fixture_read(&output);
        let terminal = state.sessions[state.find(&candidate).unwrap()].terminal.as_ref().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !terminal.screen_text(60).contains("Confirm this action?") {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        state.poll_chief_replacement();
        // Give the fake PTY time to publish a key if one was dispatched.
        std::thread::sleep(Duration::from_millis(150));
        assert!(!PathBuf::from(format!("{}.key", output.display())).exists());
        assert_eq!(state.chief_session_id(), Some(chief.as_str()));
    });
}

#[test]
fn chief_codex_first_turn_can_start_before_its_root_hook() {
    with_app("app::chief_provider_tests::chief_codex_first_turn_can_start_before_its_root_hook", |app, _| {
        let state = &mut *app.runtime;
        let chief = start_switch_fixture(state);
        let candidate = state.set_chief_model(ChiefAgent::Codex, "gpt-6.2-astra", Some(&chief), false).unwrap();
        fixture_launch(state, &candidate);
        let screen = "› Ask Codex to do anything\n  GPT-6.2-Astra default · /project";
        let output = fixture_send(state, &candidate, "composer", json!({"screen":screen}));
        fixture_read(&output);
        let index = state.find(&candidate).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !state.sessions[index].terminal.as_ref().unwrap().screen_text(60).contains("GPT-6.2-Astra") {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        state.poll_chief_replacement();
        let prompt = state.sessions[index].pending.as_ref().expect("the first turn was sent").text.clone();
        assert_eq!(state.chief_session_id(), Some(chief.as_str()));
        fixture_read(&fixture_send(state, &candidate, "start", json!({"hook_event_name":"SessionStart"})));
        fixture_read(&fixture_send(
            state,
            &candidate,
            "prompt",
            json!({"hook_event_name":"UserPromptSubmit","prompt":prompt}),
        ));
        fixture_read(&fixture_send(state, &candidate, "stop", json!({"hook_event_name":"Stop"})));
        tick_until(state, |state| state.chief_session_id() == Some(candidate.as_str()));
    });
}

#[test]
fn chief_exit_releases_its_hook_registration() {
    with_app("app::chief_provider_tests::chief_exit_releases_its_hook_registration", |app, _| {
        let state = &mut *app.runtime;
        let chief = start_switch_fixture(state);
        fixture_send(state, &chief, "exit", json!({"exit":true}));
        tick_until(state, |state| state.sessions[0].status == AgentStatus::Exited);
        assert!(!state.hub.lock().unwrap().chief_registered(&chief));
    });
}

#[test]
fn chief_restart_checks_ticket_masks_before_launching_a_different_provider() {
    with_app(
        "app::chief_provider_tests::chief_restart_checks_ticket_masks_before_launching_a_different_provider",
        |app, _| {
            let state = &mut *app.runtime;
            let fake = crate::mcp_client::FakeOAuthServer::start();
            let (dir, oauth) = crate::mcp_client::oauth_test_machine();
            state.config.tickets = toml::from_str("system = 'echo'").unwrap();
            state.config.tickets.server = fake.server();
            state.apply_ticket_config();
            state.tickets.sign.override_oauth(oauth);
            std::fs::write(dir.path().join("fail-keychain"), "").unwrap();
            install_socket_chief(state, ChiefAgent::Claude, &["haiku"]);
            state.config.chief.command = state.config.agent("claude").unwrap().command.clone();
            state.config.chief.cwd = Some(state.launch_dir.to_string_lossy().into_owned());
            let mut prior = session("prior", AgentKind::Generic, AgentStatus::Idle, &state.launch_dir);
            prior.chief = true;
            prior.agent = "codex".into();
            state.store.restart = Some(atomics_core::store::RestartState {
                sessions: vec![state.session_record(&prior, "prior-thread".into(), None)],
                ..Default::default()
            });
            state.restore_restart();
            tick_until(state, |state| state.sessions.first().is_some_and(|s| s.status == AgentStatus::Exited));
            assert!(state.sessions.iter().all(|session| session.terminal.is_none()));
            assert!(state.toast.as_ref().unwrap().0.contains("handoff credentials"));
            state.store = atomics_core::store::Store::load(&paths::store_file());
            std::fs::remove_file(dir.path().join("fail-keychain")).unwrap();
            let retry = state.start_chief(None).unwrap();
            let launch = fixture_launch(state, &retry);
            assert!(launch.to_string().contains("previous chief"));
        },
    );
}

#[test]
fn chief_restart_resumes_the_same_provider_with_an_unavailable_keychain() {
    with_app(
        "app::chief_provider_tests::chief_restart_resumes_the_same_provider_with_an_unavailable_keychain",
        |app, _| {
            let state = &mut *app.runtime;
            let fake = crate::mcp_client::FakeOAuthServer::start();
            let (dir, oauth) = crate::mcp_client::oauth_test_machine();
            state.config.tickets = toml::from_str("system = 'echo'").unwrap();
            state.config.tickets.server = fake.server();
            state.apply_ticket_config();
            state.tickets.sign.override_oauth(oauth);
            std::fs::write(dir.path().join("fail-keychain"), "").unwrap();
            install_socket_chief(state, ChiefAgent::Claude, &["haiku"]);
            state.config.chief.command = state.config.agent("claude").unwrap().command.clone();
            state.config.chief.cwd = Some(state.launch_dir.to_string_lossy().into_owned());
            let mut prior = session("prior", AgentKind::Claude, AgentStatus::Idle, &state.launch_dir);
            prior.chief = true;
            state.store.restart = Some(atomics_core::store::RestartState {
                sessions: vec![state.session_record(&prior, "prior-thread".into(), None)],
                ..Default::default()
            });
            state.restore_restart();
            tick_until(state, |state| {
                state
                    .sessions
                    .first()
                    .is_some_and(|session| session.terminal.is_some() || session.status == AgentStatus::Exited)
            });
            assert!(state.sessions[0].terminal.is_some(), "resume failed: {:?}", state.toast);
            let launch = fixture_launch(state, "prior");
            assert!(launch["args"].as_array().unwrap().windows(2).any(|pair| pair == ["--resume", "prior-thread"]));
        },
    );
}
