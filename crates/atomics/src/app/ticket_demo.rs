// Included in tickets_panel::tests so the demo uses the panel's existing egui driver.

fn demo_wait(app: &mut Desktop<'_>, until: impl Fn(&State) -> bool) {
    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(30));
    while !until(app.runtime) {
        app.runtime.poll_signin();
        app.runtime.ticket_results();
        assert!(Instant::now() < deadline, "demo did not reach the next step");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn demo_hook(
    phase: &str,
    tool: &str,
    input: serde_json::Value,
    response: serde_json::Value,
) -> crate::ipc::HookMessage {
    crate::ipc::HookMessage {
        pane: Some("chief".into()),
        ancestors: Vec::new(),
        delivered: 0,
        approval_reply: None,
        event: atomics_core::hook::HookEvent::from_payload(&serde_json::json!({
            "hook_event_name": phase, "session_id": "chief", "tool_name": format!("mcp__atomics__{tool}"),
            "tool_use_id": tool, "tool_input": input, "tool_response": response,
        }))
        .unwrap(),
    }
}

/// A scripted chief uses the real MCP-to-control adapter and worker, then delivers Claude's hooks.
fn demo_call(app: &mut Desktop<'_>, tool: &str, input: serde_json::Value) -> serde_json::Value {
    use atomics_core::trace::TraceState;
    use serde_json::{Value, json};

    app.runtime.on_hook(demo_hook("PreToolUse", tool, input.clone(), Value::Null));
    demo_wait(app, |r| r.tickets.trace_jobs.is_empty());
    assert_eq!(app.runtime.lane_entries().last().unwrap().state, TraceState::Running);
    if tool == "atomics_plan_set" {
        assert_eq!(app.runtime.lane_entries().last().unwrap().plan.as_ref().unwrap().tasks.len(), 6);
    }
    let request = crate::mcp::request_for_tool(tool, input.clone()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    app.runtime.control(
        &app.runtime.ctx.clone(),
        serde_json::to_value(request).and_then(serde_json::from_value).unwrap(),
        tx,
    );
    settle(app);
    let crate::ipc::ControlOutcome::Done(reply) =
        rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(2))).unwrap()
    else {
        panic!("demo chief's call was deferred")
    };
    assert!(reply.ok, "{reply:?}");
    app.runtime.on_hook(demo_hook("PostToolUse", tool, input, json!({"structuredContent": reply.result})));
    demo_wait(app, |r| r.tickets.trace_jobs.is_empty());
    assert_eq!(app.runtime.lane_entries().last().unwrap().state, TraceState::Ok);
    reply.result
}

#[test]
fn ticket_demo_sign_in_search_read_plan_trace_and_board() {
    use crate::mcp_client::{FakeOAuthServer, McpClient};
    use crate::ticket_signin::SignState;
    use atomics_core::trace::{TraceOp, TraceState};
    use serde_json::{Value, json};

    with_app("app::tickets_panel::tests::ticket_demo_sign_in_search_read_plan_trace_and_board", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        let state = dir.path().join("demo.json");
        let mut config = example_profile("meaning-example", &state);
        // The same fake ticket process serves every UI/chief read, behind the fake OAuth server.
        let client = McpClient::connect(&config.server, Duration::from_secs(10), |_| None).unwrap();
        let fake = FakeOAuthServer::start_with_tickets(client);
        let (_keychain, oauth) = crate::mcp_client::oauth_test_machine();
        config.server = fake.server();
        app.runtime.config.tickets = config;
        app.runtime.apply_ticket_config();
        app.runtime.tickets.sign.override_oauth(oauth.clone());
        app.runtime.tickets.sign.headless = true;
        quiet_chief(app);
        let before = app.runtime.plans.len();

        // 0:00: the settled Tickets binding opens the sign-in banner, without a host browser.
        app.runtime.diff_open = false;
        let chord = crate::theme::shortcut("cmd+k").unwrap().modifiers;
        frame(app, ctx, press(Key::K, chord));
        app.runtime.poll_signin();
        assert!(app.runtime.panel == PanelMode::Tickets && app.runtime.diff_open);
        assert_eq!(app.runtime.tickets.sign.state(), Some(&SignState::SignedOut));
        assert!(texts(&frame(app, ctx, vec![])).join("\n").contains("is not signed in"));
        // A real unsigned worker request is refused; the protected fixture sees no ticket calls.
        app.runtime.tickets.panel.query = "personal-agent ticket workspace".into();
        app.runtime.tickets_search();
        settle(app);
        assert!(app.runtime.tickets.panel.signin);
        assert!(app.runtime.tickets.panel.find.error.is_none());
        let saved: Value = serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
        assert!(saved["calls"].as_array().unwrap().is_empty());
        app.runtime.tickets.panel.query.clear();

        // 0:08: DCR + PKCE, then emulate a remote browser stopping at the callback address.
        app.runtime.start_sign_in();
        assert_eq!(app.runtime.tickets.sign.state(), Some(&SignState::SigningIn));
        demo_wait(app, |r| r.tickets.sign.job.as_ref().is_some_and(|j| j.progress.url.is_some()));
        let url = app.runtime.tickets.sign.job.as_ref().unwrap().progress.url.clone().unwrap();
        let agent: ureq::Agent =
            ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0).build().into();
        let response = agent.get(&url).call().unwrap();
        assert_eq!(response.status(), 302);
        let callback = response.headers().get("location").unwrap().to_str().unwrap().to_string();
        let mut wrong = url::Url::parse(&callback).unwrap();
        wrong.query_pairs_mut().clear().append_pair("code", "wrong").append_pair("state", "wrong");
        app.runtime.tickets.sign.paste = wrong.into();
        app.runtime.paste_signin();
        demo_wait(app, |r| r.tickets.sign.job.as_ref().is_some_and(|j| j.progress.rejected.is_some()));
        assert!(fake.state.lock().unwrap().token_requests.is_empty());
        app.runtime.tickets.sign.paste = callback;
        app.runtime.paste_signin();
        demo_wait(app, |r| r.tickets.sign.job.is_none());
        assert_eq!(app.runtime.tickets.sign.state(), Some(&SignState::SignedIn));
        assert!(!app.runtime.tickets.panel.signin);
        let signed = texts(&frame(app, ctx, vec![])).join("\n");
        assert!(signed.contains("signed in"), "{signed}");
        {
            let auth = fake.state.lock().unwrap();
            assert_eq!(auth.registrations.len(), 1);
            assert_eq!(auth.authorize_urls.len(), 1);
            assert_eq!(auth.token_requests[0]["grant_type"], "authorization_code");
            assert_eq!(auth.token_requests[0]["resource"], fake.mcp_url);
        }

        // 0:30: the query box drives meaning search. The first scored hit is the seeded epic.
        let query = "personal-agent ticket workspace";
        frame(app, ctx, vec![egui::Event::Text(query.into())]);
        frame(app, ctx, press(Key::Enter, egui::Modifiers::NONE));
        settle(app);
        let find = &app.runtime.tickets.panel.find;
        assert!(find.error.is_none(), "{:?}", find.error);
        assert_eq!(find.ran, Some((query.into(), true)));
        assert_eq!(find.results.len(), 5);
        assert_eq!(find.results[0].link.label(), "DEMO-1");
        assert_eq!(find.results[0].score, Some(0.82));
        assert!(find.results.iter().all(|c| c.snippet.is_some() && c.assignee == Some(Some("Demo User".into()))));
        let find_text = texts(&frame(app, ctx, vec![])).join("\n");
        assert!(find_text.contains("5 tickets by meaning") && find_text.contains("DEMO-1"), "{find_text}");

        // 0:45: Enter on the result loads the full epic, its four children and related matches.
        ctx.memory_mut(|m| m.request_focus(diff_view::focus_id()));
        frame(app, ctx, press(Key::Enter, egui::Modifiers::NONE));
        settle(app);
        let read = app.runtime.tickets.panel.read.as_ref().unwrap();
        assert!(read.error.is_none(), "{:?}", read.error);
        assert_eq!(read.link.label(), "DEMO-1");
        let body = read.card.body.clone().unwrap();
        assert!(body.contains("## Goal") && body.contains("## Work"));
        assert_eq!(read.card.comments.len(), 1);
        assert_eq!(read.children.items().len(), 4);
        assert_eq!(read.related.items().len(), 4);
        let children = read.children.items().to_vec();
        let read_text = texts(&frame(app, ctx, vec![])).join("\n");
        assert!(read_text.contains("Children (4)") && read_text.contains("Comments (1)"), "{read_text}");
        frame(app, ctx, press(Key::PageDown, egui::Modifiers::NONE));
        assert!(app.runtime.tickets.panel.read.as_ref().unwrap().selected.is_some());

        // 1:10: Cmd+Enter, confirm, then fill exactly this empty, unapproved plan.
        ctx.memory_mut(|m| m.request_focus(diff_view::focus_id()));
        dialog_frame(app, ctx, press(Key::Enter, plan_chord()));
        assert!(app.runtime.tickets.panel.dialog.is_some());
        dialog_frame(app, ctx, press(Key::Enter, egui::Modifiers::NONE));
        assert_eq!(app.runtime.plans.len(), before + 1);
        let plan = app.runtime.plans.last().unwrap().clone();
        assert!(plan.tasks.is_empty() && !plan.approved);
        assert_eq!(plan.ticket_mode, Some(TicketMode::Board));
        assert_eq!(super::super::sidebar::plan_tab_name(&plan), "DEMO-1 · planning…");
        let sent = sent_to_chief(app);
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains(&plan.id) && sent[0].contains("Mode: board"));
        assert!(!sent[0].contains(&body));
        assert!(app.runtime.chief_open);

        // 1:15: real chief tool calls plus injected Pre/Post hooks produce mapped cards in order.
        demo_call(app, "atomics_ticket_search", json!({"query":query, "semantic":true}));
        let read_reply = demo_call(app, "atomics_ticket_read", json!({"ticket":"DEMO-1"}));
        assert!(read_reply.to_string().contains("Ready for planning."));
        demo_call(app, "atomics_ticket_children", json!({"ticket":"DEMO-1"}));
        demo_call(app, "atomics_ticket_related", json!({"ticket":"DEMO-1"}));
        assert!(app.runtime.tickets.panel.find.from_chief);
        let mut tasks: Vec<Value> = children.iter().enumerate().map(|(i, child)| json!({
            "id":format!("t{}", i + 1), "title":child.link.title, "brief":"Implement the child ticket and check it.",
            "ticket":{"id":child.link.id, "key":child.link.key},
        })).collect();
        tasks.push(json!({"id":"t5", "title":"Connect the workspace", "brief":"Connect the four capabilities.",
            "depends_on":["t1","t2","t3","t4"]}));
        tasks.push(json!({"id":"review", "title":"Review the demo", "brief":"Check the demo and documentation.",
            "depends_on":["t5"]}));
        let reply =
            demo_call(app, "atomics_plan_set", json!({"plan":plan.id, "goal":"Ticket workspace demo", "tasks":tasks}));
        assert_eq!(reply["plan"], plan.id);
        let entries = app.runtime.lane_entries();
        assert_eq!(
            entries.iter().map(|e| e.op).collect::<Vec<_>>(),
            [TraceOp::SemanticSearch, TraceOp::Read, TraceOp::Children, TraceOp::Related, TraceOp::Plan,]
        );
        assert!(entries.iter().all(|e| e.state == TraceState::Ok));
        assert_eq!(entries[0].cards.len(), 5);
        assert_eq!(entries[1].cards[0].comments.len(), 1);
        assert_eq!(entries[2].cards.len(), 4);
        assert_eq!(entries[3].cards.len(), 4);
        assert_eq!(entries[4].plan.as_ref().unwrap().tasks.len(), 6);
        app.runtime.store.chief_dock = Some(ChiefDock::Docked);
        app.runtime.lane.height = Some(300.0);
        let mut lane_text = String::new();
        for _ in 0..2 {
            let mut lane = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 800.0))),
                    ..Default::default()
                },
                |ui| app.chief_content(ui),
            );
            lane_text = texts(&lane).join("\n");
            lane.textures_delta.clear();
        }
        for expected in
            ["TRACE", "search by meaning", "read DEMO-1", "children of DEMO-1", "related to DEMO-1", "6 tasks"]
        {
            assert!(lane_text.contains(expected), "{expected} missing from trace lane: {lane_text}");
        }

        // 1:45: the existing board renders six tasks, four ticket chips and the approval gate.
        assert_eq!(app.runtime.plans.len(), before + 1);
        let filled = app.runtime.plans.iter().find(|p| p.id == plan.id).unwrap();
        assert_eq!(filled.tasks.len(), 6);
        assert_eq!(filled.tasks.iter().filter(|t| t.ticket.is_some()).count(), 4);
        assert!(!filled.approved);
        assert_eq!(super::super::sidebar::plan_tab_name(filled), "DEMO-1 · 0/6");
        app.runtime.selected_plan = app.runtime.plans.iter().position(|p| p.id == plan.id).unwrap();
        app.runtime.panel = PanelMode::Plan;
        let board = texts(&frame(app, ctx, vec![])).join("\n");
        for expected in ["Approve plan", "DEMO-1", "DEMO-2", "DEMO-3", "DEMO-4", "DEMO-5", "Review the demo"] {
            assert!(board.contains(expected), "{expected} missing from board: {board}");
        }
        assert_eq!(app.runtime.sessions.len(), 1, "the chief must wait for approval before dispatch");
        let saved = Plan::load(&paths::atomics_home().join("plans").join(format!("{}.json", plan.id))).unwrap();
        assert_eq!(saved.tasks.len(), 6);
        assert_eq!(saved.ticket_mode, Some(TicketMode::Board));
        let fixture: Value = serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
        let calls = fixture["calls"].as_array().unwrap();
        assert!(calls.iter().any(|c| c["tool"] == "find_tickets" && c["arguments"]["about"] == query));
        assert!(calls.iter().any(|c| c["tool"] == "get_ticket" && c["arguments"]["ticket"] == "DEMO-1"));
        assert!(calls.iter().any(|c| c["tool"] == "list_ticket_children" && c["arguments"]["take"] == 50));
        let trace =
            std::fs::read_to_string(paths::atomics_home().join("trace").join(format!("{}.jsonl", plan.id))).unwrap();
        for secret in oauth.secrets(&fake.server()).unwrap() {
            assert!(!trace.contains(&secret));
        }
    });
}
