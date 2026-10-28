use super::*;

impl App {
    pub(super) fn approval_poll_interval(&self) -> Option<Duration> {
        if self.sessions.iter().any(|session| session.approval_hold || !session.approvals.is_empty()) {
            Some(Duration::from_millis(400))
        } else if self
            .sessions
            .iter()
            .any(|session| session.status == AgentStatus::NeedsInput && session.terminal.is_some())
        {
            Some(NEEDS_INPUT_CHECK_INTERVAL)
        } else {
            None
        }
    }

    pub(super) fn release_held(&mut self, index: usize) {
        if !self.sessions[index].approval_hold
            && self.sessions[index].approvals.is_empty()
            && !self.sessions[index].pending.as_ref().is_some_and(|pending| pending.codex)
            && !self.sessions[index].held.is_empty()
        {
            self.sessions[index].activity.held_since = None;
            let text = std::mem::take(&mut self.sessions[index].held).join("\n\n");
            self.deliver(index, &text);
        }
    }

    pub(super) fn expire_approvals(&mut self, now: Instant) {
        if now.duration_since(self.approvals_checked) < APPROVAL_CHECK_INTERVAL {
            return;
        }
        self.approvals_checked = now;
        let check_needs_input = now.duration_since(self.needs_input_checked) >= NEEDS_INPUT_CHECK_INTERVAL;
        if check_needs_input {
            self.needs_input_checked = now;
        }
        for index in 0..self.sessions.len() {
            let session = &mut self.sessions[index];
            let pending = session.approval_hold || !session.approvals.is_empty();
            if !pending && !(check_needs_input && session.status == AgentStatus::NeedsInput) {
                continue;
            }
            let screen = session.terminal.as_ref().map(|terminal| terminal.screen_text(60)).unwrap_or_default();
            let menu = imacs_core::approvals::menu_visible(&screen);
            if menu {
                // A native menu means Codex has already stopped waiting for the hook.
                for approval in &mut session.approvals {
                    if let Some(reply) = approval.reply.take() {
                        reply.answer(HookReply::default());
                    }
                }
                session.approval_hold = true;
                session.menu_absent_since = None;
                continue;
            }
            if session.approvals.iter().any(|pending| pending.reply.as_ref().is_some_and(|reply| !reply.expired())) {
                session.menu_absent_since = None;
                continue;
            }
            if !pending
                && !self
                    .config
                    .agent(&session.agent)
                    .and_then(|agent| agent.busy_text.as_ref())
                    .is_some_and(|busy| screen.contains(busy))
            {
                session.menu_absent_since = None;
                continue;
            }
            let missing_since = session.menu_absent_since.get_or_insert(now);
            if now.duration_since(*missing_since) < APPROVAL_MENU_GRACE {
                continue;
            }
            session.approvals.clear();
            session.approval_hold = false;
            session.menu_absent_since = None;
            if session.status == AgentStatus::NeedsInput {
                session.status = if session.terminal.as_ref().is_some_and(Terminal::exited) {
                    AgentStatus::Exited
                } else {
                    AgentStatus::Working
                };
            }
            self.release_held(index);
        }
    }

    pub(super) fn approve(&mut self, index: usize, decision: Decision, message: Option<&str>) -> Result<(), String> {
        self.expire_approvals(Instant::now());
        let session = &mut self.sessions[index];
        let Some(pending) = session.approvals.first_mut() else { return Err("no pending permission request".into()) };
        let reply = HookReply { stdout: Some(decision.output(message)) };
        let answered = pending.reply.take().is_some_and(|sender| sender.answer(reply));
        if !answered {
            let Some(terminal) = &session.terminal else {
                return Err("the permission hook has returned; answer in the agent terminal".into());
            };
            let Some(key) = imacs_core::approvals::menu_key(
                &terminal.screen_text(60),
                pending.request.command.as_deref(),
                decision,
            ) else {
                return Err("the permission menu is not visible; retry or answer in the terminal".into());
            };
            terminal.write(key.as_bytes().to_vec());
            if key == "\x01" {
                return Err("expanded the permission command; retry approval once it is visible".into());
            }
        }
        let pending = session.approvals.remove(0);
        if decision == Decision::AllowAlways {
            session.allowed.push(pending.request);
        }
        session.status = if session.approvals.is_empty() { AgentStatus::Working } else { AgentStatus::NeedsInput };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{request, session, with_app};
    use super::*;

    #[test]
    fn permission_banner_buttons_answer_hooks_and_chief_liveness_selects_the_wait() {
        with_app(
            "app::approvals::tests::permission_banner_buttons_answer_hooks_and_chief_liveness_selects_the_wait",
            |app, ctx| {
                let terminal = Terminal::spawn(
                    ctx,
                    app.palette.clone(),
                    "/bin/sleep".into(),
                    vec!["30".into()],
                    &app.launch_dir,
                    HashMap::new(),
                    100,
                )
                .unwrap();
                app.sessions = vec![session("worker", AgentKind::Generic, AgentStatus::NeedsInput, &app.launch_dir)];
                app.sessions[0].terminal = Some(terminal);
                app.record_events();
                assert!(!app.hub.lock().unwrap().chief_running);
                let mut time = 0.0;
                let mut frame = |app: &mut App, events| {
                    time += 0.05;
                    let output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1400.0, 900.0))),
                            time: Some(time),
                            events,
                            ..Default::default()
                        },
                        |ui| {
                            egui::CentralPanel::default().show(ui, |ui| {
                                let layout = layout::compute(1400.0, 900.0, 1, 0, false, 0.0, &app.config.layout);
                                app.tiles(ui, ui.max_rect(), &layout, &[0]);
                            });
                        },
                    );
                    let text: Vec<_> = output
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
                    text
                };
                for (label, decision) in
                    [("Allow", Decision::Allow), ("Allow always", Decision::AllowAlways), ("Deny", Decision::Deny)]
                {
                    let reply = Arc::new(ipc::ApprovalReply::default());
                    app.sessions[0].status = AgentStatus::NeedsInput;
                    app.sessions[0].approval_hold = true;
                    app.sessions[0].approvals.push(PendingApproval {
                        request: ApprovalRequest {
                            tool: "Bash".into(),
                            command: Some("touch banner-fixture".into()),
                            cwd: app.launch_dir.clone(),
                            reason: Some("Verify permission controls".into()),
                            input: json!({}),
                        },
                        reply: Some(reply.clone()),
                    });
                    let text = frame(app, vec![]);
                    for expected in ["Permission requested", "touch banner-fixture", "Verify permission controls"] {
                        assert!(text.iter().any(|(text, _)| text == expected), "{expected}");
                    }
                    assert!(text.iter().any(|(text, _)| text.starts_with("cwd: ")));
                    let pos = text.iter().find(|(text, _)| text == label).unwrap().1;
                    for pressed in [true, false] {
                        frame(
                            app,
                            vec![
                                egui::Event::PointerMoved(pos),
                                egui::Event::PointerButton {
                                    pos,
                                    button: egui::PointerButton::Primary,
                                    pressed,
                                    modifiers: egui::Modifiers::NONE,
                                },
                            ],
                        );
                    }
                    let result: serde_json::Value =
                        serde_json::from_str(&reply.wait(Duration::ZERO).stdout.unwrap()).unwrap();
                    assert_eq!(
                        result["hookSpecificOutput"]["decision"]["behavior"],
                        if decision == Decision::Deny { "deny" } else { "allow" }
                    );
                    assert!(app.sessions[0].approvals.is_empty());
                    assert_eq!(app.sessions[0].status, AgentStatus::Working);
                    assert!(!frame(app, vec![]).iter().any(|(text, _)| text == "Permission requested"));
                }
                assert_eq!(app.sessions[0].allowed.len(), 1);
                app.sessions[0].chief = true;
                app.record_events();
                assert!(app.hub.lock().unwrap().chief_running);
                app.sessions[0].status = AgentStatus::Exited;
                app.record_events();
                assert!(!app.hub.lock().unwrap().chief_running);
                app.sessions[0].status = AgentStatus::Working;
                app.record_events();
                app.close("worker");
                assert!(!app.hub.lock().unwrap().chief_running);
            },
        );
    }

    #[test]
    fn hidden_permission_commands_expand_before_approval_and_running_clears_needs_input() {
        with_app(
            "app::approvals::tests::hidden_permission_commands_expand_before_approval_and_running_clears_needs_input",
            |app, ctx| {
                let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex");
                app.config.agents.push(imacs_core::config::AgentConfig {
                    name: "test".into(),
                    command: vec![fixture.to_string_lossy().into_owned()],
                    kind: AgentKind::Generic,
                    busy_text: Some("esc to interrupt".into()),
                });
                let terminal = Terminal::spawn(
                    ctx,
                    app.palette.clone(),
                    fixture.to_string_lossy().into_owned(),
                    vec!["--mode".into(), "hidden".into()],
                    &app.launch_dir,
                    HashMap::new(),
                    100,
                )
                .unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                while !terminal.screen_text(60).contains("view all") {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                app.sessions = vec![session("hidden", AgentKind::Generic, AgentStatus::NeedsInput, &app.launch_dir)];
                app.sessions[0].terminal = Some(terminal);
                app.sessions[0].hooks_active = true;
                let reply = Arc::new(ipc::ApprovalReply::default());
                let permission = imacs_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name":"PermissionRequest", "session_id":"hidden", "tool_name":"Bash",
                    "tool_input":{"command":"touch fixture"}
                }))
                .unwrap();
                app.sessions[0].approval_hold = true;
                app.sessions[0].approvals.push(PendingApproval {
                    request: ApprovalRequest::from_event(&permission),
                    reply: Some(reply.clone()),
                });
                assert!(app.approve(0, Decision::Allow, None).unwrap_err().contains("expanded"));
                assert!(reply.expired() || reply.wait(Duration::ZERO).stdout.is_none());
                assert_eq!(app.sessions[0].approvals.len(), 1);
                while !app.sessions[0].terminal.as_ref().unwrap().screen_text(60).contains("$ touch fixture") {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                app.approve(0, Decision::Allow, None).unwrap();
                while !app.sessions[0].terminal.as_ref().unwrap().screen_text(60).contains("Approved") {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                app.sessions[0].approval_hold = false;
                app.sessions[0].status = AgentStatus::NeedsInput;
                app.sessions[0].menu_absent_since = Some(Instant::now() - APPROVAL_MENU_GRACE);
                app.approvals_checked = Instant::now() - APPROVAL_CHECK_INTERVAL;
                app.needs_input_checked = Instant::now() - NEEDS_INPUT_CHECK_INTERVAL;
                app.expire_approvals(Instant::now());
                assert_eq!(app.sessions[0].status, AgentStatus::Working);
                assert!(app.sessions[0].approvals.is_empty());
                assert!(!app.sessions[0].approval_hold);
                let reply = Arc::new(ipc::ApprovalReply::default());
                app.sessions[0].approvals.push(PendingApproval {
                    request: ApprovalRequest::from_event(&permission),
                    reply: Some(reply.clone()),
                });
                app.sessions[0].status = AgentStatus::NeedsInput;
                let event = imacs_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name":"PreToolUse", "session_id":"hidden", "tool_name":"Bash",
                    "tool_input":{"command":"touch fixture"}
                }))
                .unwrap();
                app.on_hook(HookMessage { pane: None, ancestors: vec![], event, delivered: 0, approval_reply: None });
                assert_eq!(app.sessions[0].status, AgentStatus::Working);
                assert!(app.sessions[0].approvals.is_empty());
                assert!(reply.wait(Duration::ZERO).stdout.is_none());
            },
        );
    }

    #[test]
    fn expired_approvals_wait_for_menu_quiet_or_lifecycle() {
        with_app("app::approvals::tests::expired_approvals_wait_for_menu_quiet_or_lifecycle", |app, ctx| {
            let dir = tempfile::tempdir().unwrap();
            app.sessions = vec![session("stale", AgentKind::Generic, AgentStatus::NeedsInput, dir.path())];
            let permission = imacs_core::hook::HookEvent::from_payload(&json!({
                "hook_event_name":"PermissionRequest", "session_id":"stale", "cwd":dir.path(),
                "tool_name":"Bash", "tool_input":{"command":"touch fixture", "description":"test"}
            }))
            .unwrap();
            let reply = Arc::new(ipc::ApprovalReply::default());
            app.sessions[0].approval_hold = true;
            app.sessions[0].approvals.push(PendingApproval {
                request: ApprovalRequest::from_event(&permission),
                reply: Some(reply.clone()),
            });
            let raw = request(app, json!({"cmd":"send", "session":"stale", "raw":true, "text":"y"}));
            assert!(!raw.ok);
            assert!(raw.error.unwrap().contains("imacs_approve"));
            assert_eq!(app.deliver(0, "held plain message"), Delivery::Held);
            let now = Instant::now();
            app.approvals_checked = now - APPROVAL_CHECK_INTERVAL;
            app.expire_approvals(now);
            assert_eq!(app.sessions[0].approvals.len(), 1);
            reply.wait(Duration::ZERO);
            let first_check = now + APPROVAL_CHECK_INTERVAL;
            app.expire_approvals(first_check);
            assert_eq!(app.sessions[0].approvals.len(), 1, "an expired hook may precede its menu");
            assert_eq!(app.sessions[0].held, ["held plain message"]);
            let second_check = first_check + APPROVAL_CHECK_INTERVAL;
            app.expire_approvals(second_check);
            assert_eq!(app.sessions[0].approvals.len(), 1, "the menu quiet period must elapse");
            app.sessions[0].menu_absent_since = Some(first_check - APPROVAL_MENU_GRACE);
            app.expire_approvals(second_check);
            assert_eq!(app.sessions[0].approvals.len(), 1, "screen checks must be throttled");
            app.expire_approvals(second_check + APPROVAL_CHECK_INTERVAL);
            assert!(app.sessions[0].approvals.is_empty());
            assert!(app.sessions[0].held.is_empty());
            assert_eq!(app.hub.lock().unwrap().outbox["stale"], ["held plain message"]);
            let menu = "Allow these file edits?\n› 1. Yes, proceed\n  2. No\nPress enter to confirm or esc to cancel";
            assert!(imacs_core::approvals::menu_key(menu, Some("touch fixture"), Decision::Allow).is_none());
            let terminal = Terminal::spawn(
                ctx,
                app.palette.clone(),
                "/bin/sh".into(),
                vec!["-c".into(), "printf '%s' \"$1\"; exec sleep 10".into(), "fixture".into(), menu.into()],
                dir.path(),
                HashMap::new(),
                100,
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while !terminal.screen_text(60).contains("esc to cancel") {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            app.sessions[0].terminal = Some(terminal);
            app.sessions[0].status = AgentStatus::NeedsInput;
            app.sessions[0].approval_hold = true;
            app.sessions[0].menu_absent_since = Some(Instant::now() - APPROVAL_MENU_GRACE);
            app.sessions[0]
                .approvals
                .push(PendingApproval { request: ApprovalRequest::from_event(&permission), reply: Some(reply) });
            assert_eq!(app.deliver(0, "held while editing"), Delivery::Held);
            let before = app.sessions[0].terminal.as_ref().unwrap().screen_text(60);
            app.approvals_checked = Instant::now() - APPROVAL_CHECK_INTERVAL;
            app.expire_approvals(Instant::now());
            assert_eq!(app.sessions[0].approvals.len(), 1);
            assert!(app.sessions[0].menu_absent_since.is_none(), "a menu drawn late resets the quiet period");
            assert_eq!(app.sessions[0].held, ["held while editing"]);
            let event = imacs_core::hook::HookEvent::from_payload(&json!({"hook_event_name":"PreToolUse", "session_id":"stale", "cwd":dir.path(), "tool_name":"Bash", "tool_input":{"command":"cat fixture"}})).unwrap();
            app.on_hook(HookMessage { pane: None, ancestors: vec![], event, delivered: 0, approval_reply: None });
            assert_eq!(app.sessions[0].approvals.len(), 1, "an unrelated tool must leave the permission pending");
            assert_eq!(app.sessions[0].status, AgentStatus::NeedsInput);
            assert!(app.sessions[0].approval_hold, "PreToolUse alone must not release input into a menu");
            assert_eq!(app.deliver(0, "still held"), Delivery::Held);
            app.approvals_checked = Instant::now() - APPROVAL_CHECK_INTERVAL;
            app.expire_approvals(Instant::now());
            assert_eq!(app.sessions[0].held.len(), 2);
            assert_eq!(app.sessions[0].terminal.as_ref().unwrap().screen_text(60), before);
            app.sessions[0].terminal = None;
            let event = imacs_core::hook::HookEvent::from_payload(
                &json!({"hook_event_name":"Interrupt", "session_id":"stale", "cwd":dir.path()}),
            )
            .unwrap();
            app.on_hook(HookMessage { pane: None, ancestors: vec![], event, delivered: 0, approval_reply: None });
            assert!(!app.sessions[0].approval_hold);
            assert!(app.sessions[0].held.is_empty());
            assert!(
                app.hub.lock().unwrap().outbox["stale"].last().unwrap().contains("held while editing\n\nstill held")
            );
            app.sessions.clear();
        });
    }

    #[test]
    fn approvals_and_held_sends_stay_with_their_plan_across_tab_switches() {
        with_app(
            "app::approvals::tests::approvals_and_held_sends_stay_with_their_plan_across_tab_switches",
            |app, ctx| {
                app.config.agents.push(imacs_core::config::AgentConfig {
                    name: "test".into(),
                    command: vec![],
                    kind: AgentKind::Generic,
                    busy_text: Some("esc to interrupt".into()),
                });
                let dir = tempfile::tempdir().unwrap();
                let mut indices = Vec::new();
                for (goal, id) in [("One", "worker-one"), ("Two", "worker-two")] {
                    assert!(
                        request(app, json!({"cmd":"plan_set", "goal":goal, "tasks":[{"id":"a", "title":goal}]})).ok
                    );
                    let index = app.selected_plan;
                    app.plans[index].tasks[0].session = Some(id.into());
                    app.plans[index].tasks[0].status = TaskStatus::Running;
                    indices.push(index);
                    let mut worker = session(id, AgentKind::Generic, AgentStatus::Working, dir.path());
                    worker.claude_id = Some(format!("codex-{id}"));
                    worker.terminal = Some(
                        Terminal::spawn(
                            ctx,
                            app.palette.clone(),
                            "/bin/cat".into(),
                            vec![],
                            dir.path(),
                            HashMap::new(),
                            100,
                        )
                        .unwrap(),
                    );
                    app.sessions.push(worker);
                }
                let (one, two) = (indices[0], indices[1]);
                let hook = |session: &str, name: &str| {
                    HookMessage {
                pane: Some(session.into()), ancestors: Vec::new(), delivered: 0, approval_reply: None,
                event: imacs_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name":name, "session_id":format!("codex-{session}"), "cwd":dir.path(),
                    "tool_name":"Bash", "tool_input":{"command":"touch permission-fixture", "description":format!("Request from {session}")}
                })).unwrap(),
            }
                };
                let permission = |session: &str| {
                    let reply = Arc::new(ipc::ApprovalReply::default());
                    let mut event = hook(session, "PermissionRequest");
                    event.approval_reply = Some(reply.clone());
                    (event, reply)
                };
                app.record_events();
                let (wait_tx, wait_rx) = channel();
                app.control(
                    ctx,
                    ControlRequest::Wait {
                        sessions: vec!["worker-one".into()],
                        statuses: vec!["needs you".into()],
                        after: Some(app.event_seq),
                        timeout_secs: Some(30),
                    },
                    wait_tx,
                );
                let (event, second_reply) = permission("worker-two");
                app.on_hook(event);
                assert!(wait_rx.try_recv().is_err(), "another plan's approval must not satisfy this waiter");
                let (event, first_reply) = permission("worker-one");
                app.on_hook(event);
                let ControlOutcome::Done(waited) = wait_rx.recv_timeout(Duration::from_secs(2)).unwrap() else {
                    panic!("wait response")
                };
                let event = &waited.result["events"][0];
                assert_eq!(event["plan"], app.plans[one].id);
                assert_eq!(event["task"], "a");
                assert_eq!(event["session"], "worker-one");
                assert_eq!(event["approval"]["command"], "touch permission-fixture");
                assert_eq!(event["approval"]["reason"], "Request from worker-one");
                assert_eq!(event["approval"]["cwd"], json!(dir.path()));
                let other =
                    app.events.iter().find(|e| e.session == "worker-two" && e.approval.is_some()).unwrap().to_json();
                assert_eq!(other["plan"], app.plans[two].id);
                assert_eq!(other["task"], "a");
                assert_eq!(other["approval"]["reason"], "Request from worker-two");
                app.last_status.remove("worker-one");
                app.record_events();
                let event = app.events.last().unwrap().to_json();
                assert_eq!(event["plan"], app.plans[one].id);
                assert_eq!(event["task"], "a");
                assert_eq!(event["approval"]["command"], "touch permission-fixture");
                for (index, id) in [(0, "worker-one"), (1, "worker-two")] {
                    let response = request(app, json!({"cmd":"send", "session":id, "text":format!("steer-{id}")}));
                    assert!(response.ok);
                    assert_eq!(response.result["held"], true);
                    assert_eq!(response.result["sent"], false);
                    let raw =
                        request(app, json!({"cmd":"send", "session":id, "text":format!("raw-{id}\\r"), "raw":true}));
                    assert!(!raw.ok);
                    assert!(raw.error.unwrap().contains("imacs_approve"));
                    assert_eq!(app.sessions[index].held.len(), 1);
                    assert!(app.sessions[index].pending.is_none());
                }
                assert_eq!(app.selected_plan, two);
                assert!(request(app, json!({"cmd":"approve", "session":"worker-one", "decision":"allow_always", "message":"First plan approved"})).ok);
                let output: serde_json::Value =
                    serde_json::from_str(&first_reply.wait(Duration::ZERO).stdout.unwrap()).unwrap();
                assert_eq!(output["hookSpecificOutput"]["decision"]["behavior"], "allow");
                assert_eq!(output["hookSpecificOutput"]["decision"]["message"], "First plan approved");
                assert!(app.sessions[0].approvals.is_empty());
                assert_eq!(app.sessions[1].approvals.len(), 1);
                assert_eq!(app.sessions[1].status, AgentStatus::NeedsInput);
                assert_eq!(
                    app.sessions[0].held.len(),
                    1,
                    "approval alone must not flush sends into the permission menu"
                );
                app.on_hook(hook("worker-one", "PostToolUse"));
                assert!(app.sessions[0].held.is_empty());
                assert_eq!(app.sessions[0].pending.as_ref().unwrap().text, "steer-worker-one");
                assert_eq!(app.sessions[1].held.len(), 1);
                assert!(app.sessions[1].pending.is_none());
                let deadline = Instant::now() + Duration::from_secs(2);
                while !app.sessions[0].terminal.as_ref().unwrap().screen_text(20).contains("steer-worker-one") {
                    assert!(Instant::now() < deadline, "first plan's plain message was not released");
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(!app.sessions[0].terminal.as_ref().unwrap().screen_text(20).contains("raw-worker-one"));
                assert!(!app.sessions[1].terminal.as_ref().unwrap().screen_text(20).contains("worker-one"));
                app.selected_plan = one;
                app.show_plan_fleet();
                assert!(
                request(
                    app,
                    json!({"cmd":"approve", "session":"worker-two", "decision":"deny", "message":"Second plan denied"})
                )
                .ok
            );
                let output: serde_json::Value =
                    serde_json::from_str(&second_reply.wait(Duration::ZERO).stdout.unwrap()).unwrap();
                assert_eq!(output["hookSpecificOutput"]["decision"]["behavior"], "deny");
                assert_eq!(output["hookSpecificOutput"]["decision"]["message"], "Second plan denied");
                assert_eq!(app.sessions[1].held.len(), 1);
                app.on_hook(hook("worker-two", "Interrupt"));
                assert!(app.sessions[1].held.is_empty());
                assert_eq!(app.sessions[1].pending.as_ref().unwrap().text, "steer-worker-two");
                assert_eq!(app.sessions[0].pending.as_ref().unwrap().text, "steer-worker-one");
                let (event, reply) = permission("worker-two");
                app.on_hook(event);
                assert_eq!(app.sessions[1].approvals.len(), 1, "allow_always must not leak to another plan's session");
                assert!(request(app, json!({"cmd":"approve", "session":"worker-two", "decision":"allow"})).ok);
                assert!(reply.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
                let (event, reply) = permission("worker-one");
                app.on_hook(event);
                assert!(reply.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
                assert!(app.sessions[0].approvals.is_empty());
                assert_eq!(app.plans[one].tasks[0].status, TaskStatus::Running);
                assert_eq!(app.plans[two].tasks[0].status, TaskStatus::Running);
            },
        );
    }
}
