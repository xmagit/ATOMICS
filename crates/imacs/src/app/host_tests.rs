//! These tests never run egui, call a client callback, or drive a runtime tick.
//! All progress after startup must come from the production host thread.

use super::tests::{repository, session, with_state};
use super::*;
use crate::host::Host;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

fn response(rx: Receiver<ControlOutcome>) -> ControlResponse {
    let ControlOutcome::Done(response) = rx.recv_timeout(Duration::from_secs(10)).expect("host did not answer") else {
        panic!("expected completed command");
    };
    assert!(response.ok, "{:?}", response.error);
    response
}

fn command(host: &Host, value: serde_json::Value) -> ControlResponse {
    response(host.command(serde_json::from_value(value).unwrap()))
}

fn eventually(host: &Host, check: impl Fn(&State) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if check(&host.state()) {
            break;
        }
        assert!(Instant::now() < deadline, "host did not make progress without frames");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn hook(name: &str, tool: Option<&str>) -> HookReply {
    let stream = UnixStream::connect(paths::socket_path()).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    writeln!(
        &stream,
        "{}",
        json!({
            "pane":"worker", "ancestors":[],
            "payload":{
                "hook_event_name":name, "session_id":"worker", "cwd":paths::imacs_home(),
                "tool_name":tool, "tool_input":{"command":"git status"}
            }
        })
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn expired_approval(state: &mut State) -> Arc<ipc::ApprovalReply> {
    let reply = Arc::new(ipc::ApprovalReply::default());
    let mut worker = session("worker", AgentKind::Claude, AgentStatus::NeedsInput, &state.launch_dir);
    worker.approval_hold = true;
    worker.approvals.push(PendingApproval {
        id: uuid::Uuid::new_v4().to_string(),
        request: ApprovalRequest {
            tool: "Bash".into(),
            command: Some("touch pending".into()),
            cwd: state.launch_dir.clone(),
            reason: None,
            input: json!({}),
        },
        reply: Some(reply.clone()),
    });
    state.sessions = vec![worker];
    reply
}

#[test]
fn permission_hook_gets_its_reply_without_frames() {
    with_state("app::host_tests::permission_hook_gets_its_reply_without_frames", |mut state, _| {
        state.sessions = vec![session("worker", AgentKind::Claude, AgentStatus::Working, &state.launch_dir)];
        let host = state.into_host();
        let reply = hook("PermissionRequest", Some("Bash"));
        assert!(reply.stdout.unwrap().contains("allow"));
        assert_eq!(host.state().sessions.len(), 1);
    });
}

#[test]
fn approval_expires_without_frames() {
    with_state("app::host_tests::approval_expires_without_frames", |mut state, _| {
        let reply = expired_approval(&mut state);
        let host = state.into_host();
        // Simulate the hook's own timeout after the runtime is already running.
        assert!(reply.wait(Duration::ZERO).stdout.is_none());
        eventually(&host, |state| !state.sessions[0].approval_hold && state.sessions[0].approvals.is_empty());
        assert_eq!(host.state().sessions[0].status, AgentStatus::Working);
    });
}

#[test]
fn held_messages_release_without_frames() {
    with_state("app::host_tests::held_messages_release_without_frames", |mut state, _| {
        let reply = expired_approval(&mut state);
        state.sessions[0].held = vec!["held message".into()];
        let hub = state.hub.clone();
        let host = state.into_host();
        reply.wait(Duration::ZERO);
        eventually(&host, |state| state.sessions[0].held.is_empty());
        assert_eq!(hub.lock().unwrap().outbox["worker"], ["held message"]);
    });
}

#[test]
fn waiter_wakes_on_a_hook_status_change_without_frames() {
    with_state("app::host_tests::waiter_wakes_on_a_hook_status_change_without_frames", |mut state, _| {
        state.sessions = vec![session("worker", AgentKind::Claude, AgentStatus::Working, &state.launch_dir)];
        let host = state.into_host();
        let wait = host.command(ControlRequest::Wait {
            sessions: vec!["worker".into()],
            statuses: vec!["turn done".into()],
            after: Some(0),
            timeout_secs: Some(10),
        });
        // A following command is a FIFO barrier: the waiter has been registered.
        command(&host, json!({"cmd":"list"}));
        hook("Stop", None);
        let reply = response(wait);
        assert_eq!(reply.result["timed_out"], false);
        assert_eq!(reply.result["events"][0]["session"], "worker");
        assert_eq!(reply.result["events"][0]["status"], "turn done");
    });
}

#[test]
fn waiter_times_out_on_its_own_deadline_without_frames() {
    with_state("app::host_tests::waiter_times_out_on_its_own_deadline_without_frames", |state, _| {
        let host = state.into_host();
        let reply = command(&host, json!({"cmd":"wait", "after":0, "timeout_secs":1}));
        assert_eq!(reply.result["timed_out"], true);
        assert!(reply.result["events"].as_array().unwrap().is_empty());
    });
}

#[test]
fn heartbeat_fires_without_frames() {
    with_state("app::host_tests::heartbeat_fires_without_frames", |mut state, _| {
        state.config.chief.heartbeat_secs = 1;
        let host = state.into_host();
        let reply = command(&host, json!({"cmd":"wait", "statuses":["heartbeat"], "after":0, "timeout_secs":5}));
        assert_eq!(reply.result["timed_out"], false);
        assert_eq!(reply.result["events"][0]["status"], "heartbeat");
        assert!(host.state().heartbeat.last.is_some());
    });
}

#[test]
fn plan_is_saved_without_frames() {
    with_state("app::host_tests::plan_is_saved_without_frames", |state, _| {
        let _host = state.into_host();
        let reply = ipc::request(
            &serde_json::from_value(json!({
                "cmd":"plan_set", "goal":"Saved without painting", "tasks":[{"id":"a"}]
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(reply.ok, "{:?}", reply.error);
        let (plans, errors) = plan::load_plans(&paths::imacs_home()).unwrap();
        assert!(errors.is_empty());
        let saved = plans.iter().find(|p| json!(p.id) == reply.result["plan"]).unwrap();
        assert_eq!(saved.goal, "Saved without painting");
        assert_eq!(saved.tasks[0].id, "a");
    });
}

#[test]
fn integration_finishes_and_persists_without_frames() {
    with_state("app::host_tests::integration_finishes_and_persists_without_frames", |mut state, _| {
        let (_dir, repo) = repository();
        let worktree = repo.with_file_name("worker");
        git::add_worktree(&repo, &worktree, "task-a", "main").unwrap();
        std::fs::write(worktree.join("result.txt"), "completed work").unwrap();
        state.plans = vec![Plan {
            id: "no-frames".into(),
            project: Some(repo.clone()),
            approved: true,
            tasks: vec![Task {
                id: "a".into(),
                branch: Some("task-a".into()),
                worktree: Some(worktree),
                status: TaskStatus::Review,
                ..Task::default()
            }],
            ..Plan::default()
        }];
        state.selected_plan = 0;
        state.config.worktree_path = repo.with_file_name("worktrees").to_string_lossy().into_owned();
        let into = state.plans[0].integration_branch();
        let host = state.into_host();
        let result = command(&host, json!({"cmd":"plan_integrate", "plan":"no-frames", "id":"a"}));
        assert_eq!(result.result["integration"], into);
        assert_eq!(git::git(&repo, &["show", &format!("{into}:result.txt")]).unwrap().trim(), "completed work");
        let saved = plan::load_plans(&paths::imacs_home()).unwrap().0;
        assert_eq!(saved[0].tasks[0].integrated.as_deref(), Some(into.as_str()));
    });
}

#[test]
fn agent_exit_updates_and_saves_its_plan_without_frames() {
    with_state("app::host_tests::agent_exit_updates_and_saves_its_plan_without_frames", |mut state, _ctx| {
        let terminal = Terminal::spawn(
            state.wake.clone(),
            state.palette.terminal_colors(),
            "/bin/sh".into(),
            vec!["-c".into(), "sleep 0.1".into()],
            &state.launch_dir,
            HashMap::new(),
            100,
        )
        .unwrap();
        state.sessions = vec![session("worker", AgentKind::Generic, AgentStatus::Working, &state.launch_dir)];
        state.sessions[0].terminal = Some(terminal);
        state.plans = vec![Plan {
            id: "agent-exit".into(),
            tasks: vec![Task {
                id: "a".into(),
                session: Some("worker".into()),
                status: TaskStatus::Running,
                ..Task::default()
            }],
            ..Plan::default()
        }];
        let host = state.into_host();
        eventually(&host, |state| state.plans[0].tasks[0].status == TaskStatus::Blocked);
        let saved = plan::load_plans(&paths::imacs_home()).unwrap().0;
        assert_eq!(saved[0].tasks[0].status, TaskStatus::Blocked);
    });
}

#[test]
fn delivery_verification_times_out_without_frames() {
    with_state("app::host_tests::delivery_verification_times_out_without_frames", |mut state, _ctx| {
        let terminal = Terminal::spawn(
            state.wake.clone(),
            state.palette.terminal_colors(),
            "/bin/sleep".into(),
            vec!["30".into()],
            &state.launch_dir,
            HashMap::new(),
            100,
        )
        .unwrap();
        let (reply, received) = channel();
        let mut worker = session("worker", AgentKind::Generic, AgentStatus::Idle, &state.launch_dir);
        worker.terminal = Some(terminal);
        worker.pending = Some(Pending {
            text: "unconfirmed message".into(),
            sent: Instant::now() - DELIVERY_TIMEOUT + Duration::from_millis(100),
            attempts: DELIVERY_ATTEMPTS,
            was_working: false,
            codex: false,
            screen_before: String::new(),
            paste_seen: false,
            reply: Some(reply),
        });
        state.sessions = vec![worker];
        let host = state.into_host();
        let ControlOutcome::Done(result) = received.recv_timeout(Duration::from_secs(3)).unwrap() else {
            panic!("delivery was not resolved");
        };
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("no receipt"));
        assert!(host.state().sessions[0].pending.is_none());
    });
}

#[test]
fn worktree_changes_publish_a_fresh_diff_without_frames() {
    with_state("app::host_tests::worktree_changes_publish_a_fresh_diff_without_frames", |mut state, _| {
        let (_dir, repo) = repository();
        state.config.base_ref = "main".into();
        state.repos = vec![repo.clone()];
        state.diff_root = Some(repo.clone());
        let host = state.into_host();
        eventually(&host, |state| state.diffs.get(&repo).is_some_and(|d| d.prepared.is_some()));
        // The watcher is installed and the first revision has already been published.
        // Only a real filesystem change may produce the next revision.
        let initial = host.state().diffs[&repo].prepared.clone().unwrap();
        std::fs::write(repo.join("new.rs"), "fn fresh_revision() {}\n").unwrap();
        eventually(&host, |state| {
            state.diffs[&repo]
                .prepared
                .as_ref()
                .is_some_and(|p| !Arc::ptr_eq(&initial, p) && p.diff.files.iter().any(|file| file.path() == "new.rs"))
        });
        let current = host.state().diffs[&repo].prepared.clone().unwrap();
        std::fs::write(repo.join("new.rs"), "fn changed_again() {}\nfn another_line() {}\n").unwrap();
        eventually(&host, |state| {
            state.diffs[&repo].prepared.as_ref().is_some_and(|p| !Arc::ptr_eq(&current, p) && p.additions == 2)
        });
    });
}

#[test]
fn held_terminal_input_gets_a_receipt_without_frames() {
    with_state("app::host_tests::held_terminal_input_gets_a_receipt_without_frames", |mut state, _| {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex");
        state.config.agents.push(imacs_core::config::AgentConfig {
            name: "fake-codex".into(),
            command: vec![fixture.to_string_lossy().into_owned()],
            kind: AgentKind::Generic,
            busy_text: None,
        });
        let reply = expired_approval(&mut state);
        let worker = &mut state.sessions[0];
        worker.agent = "fake-codex".into();
        worker.kind = AgentKind::Generic;
        worker.held = vec!["A message held until the approval expires".into()];
        worker.terminal = Some(
            Terminal::spawn(
                state.wake.clone(),
                state.palette.terminal_colors(),
                fixture.to_string_lossy().into_owned(),
                vec!["--mode".into(), "queue".into()],
                &state.launch_dir,
                HashMap::new(),
                100,
            )
            .unwrap(),
        );
        let host = state.into_host();
        eventually(&host, |state| {
            state.sessions[0].terminal.as_ref().unwrap().screen_text(60).contains("tab to queue")
        });
        reply.wait(Duration::ZERO);
        eventually(&host, |state| {
            let worker = &state.sessions[0];
            worker.held.is_empty()
                && worker.pending.is_none()
                && steer::message_in_transcript(
                    &worker.terminal.as_ref().unwrap().screen_text(60),
                    "A message held until the approval expires",
                )
        });
        assert!(host.state().sessions[0].last_message.is_none());
    });
}

#[test]
fn cleanup_previews_finish_without_painting_the_dialogs() {
    with_state("app::host_tests::cleanup_previews_finish_without_painting_the_dialogs", |mut state, _| {
        let (_dir, repo) = repository();
        state.config.base_ref = "main".into();
        state.plans[0].project = Some(repo.clone());
        state.plans[0].status = PlanStatus::Completed;
        state.open_cleanup(repo, None);
        state.open_plan_cleanup(0);
        let host = state.into_host();
        eventually(&host, |state| {
            state.cleanup.as_ref().unwrap().rows.is_some() && state.plan_cleanup.as_ref().unwrap().ready()
        });
    });
}

#[test]
fn desktop_actions_launch_and_deliver_on_the_host_without_frames() {
    with_state("app::host_tests::desktop_actions_launch_and_deliver_on_the_host_without_frames", |state, _| {
        let actions = state.action_tx.clone();
        let host = state.into_host();
        let (tx, rx) = channel();
        actions
            .send(Box::new(move |state| {
                assert_eq!(std::thread::current().name(), Some("imacs-runtime"));
                let id =
                    state.start(&state.ctx.clone(), "shell", state.launch_dir.clone(), None, vec![], None).unwrap();
                tx.send(id).unwrap();
            }))
            .unwrap();
        let id = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let session = id.clone();
        actions
            .send(Box::new(move |state| {
                let index = state.find(&session).unwrap();
                state.deliver(index, "printf 'desktop-action-delivered\\n'");
            }))
            .unwrap();
        eventually(&host, |state| {
            state.sessions[state.find(&id).unwrap()]
                .terminal
                .as_ref()
                .unwrap()
                .screen_text(60)
                .lines()
                .any(|line| line.trim() == "desktop-action-delivered")
        });
    });
}

#[test]
fn blocked_project_picker_does_not_block_hooks_or_waits() {
    with_state("app::host_tests::blocked_project_picker_does_not_block_hooks_or_waits", |mut state, ctx| {
        let (_dir, repo) = repository();
        state.sessions = vec![session("worker", AgentKind::Claude, AgentStatus::Working, &state.launch_dir)];
        let client = Client::new(ctx, &mut Options::default(), &crate::load_icon());
        let mut app = App { client, host: state.into_host(), replies: Vec::new() };
        // The UI can keep this request pending across frames without invoking
        // native code or draining the runtime's action queue under its guard.
        for _ in 0..2 {
            ctx.run_ui(egui::RawInput::default(), |_| {
                let mut state = app.host.state();
                Desktop { runtime: &mut state, client: &mut app.client }.pick_project();
            })
            .drop_without_applying_deltas();
        }
        let wait = app.host.command(ControlRequest::Wait {
            sessions: vec!["worker".into()],
            statuses: vec!["turn done".into()],
            after: Some(0),
            timeout_secs: Some(5),
        });
        command(&app.host, json!({"cmd":"list"}));
        let (opened_tx, opened_rx) = channel();
        let (done_tx, done_rx) = channel();
        let worker = std::thread::spawn(move || {
            opened_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            hook("Stop", None);
            let reply = response(wait);
            assert_eq!(reply.result["timed_out"], false);
            assert_eq!(reply.result["events"][0]["status"], "turn done");
            // This real deadline also has to fire while the fake picker blocks.
            let reply = ipc::request(
                &serde_json::from_value(json!({"cmd":"wait", "after":u64::MAX, "timeout_secs":1})).unwrap(),
            )
            .unwrap();
            assert_eq!(reply.result["timed_out"], true);
            done_tx.send(()).unwrap();
        });
        ctx.run_ui(egui::RawInput::default(), |ui| {
            app.ui_with_picker(ui, |_| {
                opened_tx.send(()).unwrap();
                done_rx.recv_timeout(Duration::from_secs(10)).expect("runtime stalled while folder picker was open");
                Some(repo.clone())
            });
        })
        .drop_without_applying_deltas();
        worker.join().unwrap();
        eventually(&app.host, |state| state.open_projects.contains(&repo.canonicalize().unwrap()));
        assert!(app.client.project_picker.is_none());
    });
}

#[test]
fn delayed_file_open_keeps_the_hosts_resolved_target() {
    with_state("app::host_tests::delayed_file_open_keeps_the_hosts_resolved_target", |mut state, ctx| {
        let (_dir, repo) = repository();
        let path = "review.txt";
        std::fs::write(repo.join(path), "original\n").unwrap();
        git::git(&repo, &["add", path]).unwrap();
        git::git(&repo, &["commit", "-qm", "tracked file"]).unwrap();
        std::fs::write(repo.join(path), "changed\n").unwrap();
        state.config.base_ref = "main".into();
        state.repos = vec![repo.clone()];
        state.diff_root = Some(repo.clone());
        let mut client = Client::new(ctx, &mut Options::default(), &crate::load_icon());
        let host = state.into_host();
        eventually(&host, |state| {
            state.diffs.get(&repo).and_then(|d| d.prepared.as_ref()).is_some_and(|p| !p.diff.files.is_empty())
        });
        for whole_file in [false, true] {
            {
                let mut state = host.state();
                let mut app = Desktop { runtime: &mut state, client: &mut client };
                app.open_file(&repo, path);
                // Do not drain the action or consume events across these frames.
                for _ in 0..2 {
                    ctx.run_ui(egui::RawInput::default(), |_| app.sync_diffs()).drop_without_applying_deltas();
                }
            }
            eventually(&host, |state| {
                state.client_events.iter().any(|event| matches!(event, ClientEvent::OpenFile { .. }))
            });
            assert_eq!(host.state().diffs[&repo].viewing.is_some(), whole_file);
            let before = host.state().diffs[&repo].revision;
            let contents = if whole_file { "changed again\n" } else { "original\n" };
            std::fs::write(repo.join(path), contents).unwrap();
            eventually(&host, |state| {
                let data = &state.diffs[&repo];
                data.revision > before && data.prepared.as_ref().is_some_and(|p| p.diff.files.is_empty() != whole_file)
            });
            if whole_file {
                eventually(&host, |state| {
                    state.diffs[&repo]
                        .file
                        .as_ref()
                        .is_some_and(|file| file.lines.first().is_some_and(|line| line == "changed again"))
                });
            }
            let mut state = host.state();
            let mut app = Desktop { runtime: &mut state, client: &mut client };
            for _ in 0..2 {
                ctx.run_ui(egui::RawInput::default(), |_| app.sync_diffs()).drop_without_applying_deltas();
            }
            app.apply_client_events(ctx);
            app.sync_diffs();
            let view = &app.client.diff_views[&repo];
            assert_eq!(
                view.viewing, app.runtime.diffs[&repo].viewing,
                "late events must not choose a different target"
            );
            assert_eq!(view.viewing.is_some(), whole_file);
            assert_eq!(view.focused.as_deref(), Some(path));
            if whole_file {
                assert_eq!(view.file.as_ref().unwrap().lines[0], "changed again");
            } else {
                assert!(!view.loading);
            }
        }
    });
}
