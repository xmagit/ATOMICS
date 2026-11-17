//! Host chief identity and trace integration tests. Painting lives in atomics-ui.
use super::*;
#[cfg(test)]
use atomics_core::trace::TraceEntry;
impl State {
    pub(super) fn chief_session_id(&self) -> Option<&str> {
        self.sessions.iter().find(|s| s.chief).map(|s| s.id.as_str())
    }

    /// Opens the chief's window and focuses its terminal.
    pub(super) fn show_chief(&mut self) {
        self.chief_open = true;
        self.focus_terminal = self.chief_session_id().map(str::to_string);
    }
}

impl Desktop<'_> {
    /// The chief's trace entries still in view, oldest first.
    #[cfg(test)]
    pub(super) fn lane_entries(&self) -> Vec<&TraceEntry> {
        let Some(log) = self.runtime.chief_session_id().and_then(|id| self.runtime.traces.get(id)) else {
            return Vec::new();
        };
        log.entries.iter().filter(|entry| entry.seq > self.client.ui.lane.cleared).collect()
    }

    /// The newest card as one line, for the Tickets panel's footer.
    pub(super) fn lane_echo(&self) -> Option<String> {
        self.client.ui.lane.echo(&self.runtime.trace_data())
    }
}
#[cfg(test)]
mod tests {
    use super::super::tests::{session, with_app};
    use super::*;
    use crate::ipc::HookMessage;
    use crate::mcp_client::tests::example_profile;
    use atomics_ui::trace_lane::{CardView, Glyph, card_view, focus_id};
    use serde_json::{Value, json};

    fn hook(name: &str, tool: &str, id: &str, input: Value, response: Value) -> HookMessage {
        HookMessage {
            pane: Some("chief".into()),
            ancestors: Vec::new(),
            delivered: Vec::new(),
            approval_reply: None,
            event: atomics_core::hook::HookEvent::from_payload(&json!({
                "hook_event_name":name, "session_id":"chief", "tool_name":format!("mcp__atomics__{tool}"),
                "tool_use_id":id, "tool_input":input, "tool_response":response
            }))
            .unwrap(),
        }
    }

    fn settle(app: &mut Desktop<'_>) {
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(30));
        loop {
            app.runtime.ticket_results();
            if app.runtime.tickets.pending == 0 {
                return;
            }
            assert!(Instant::now() < deadline, "ticket jobs did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The chief calls one of its ticket tools: ATOMICS runs it on the fake server and the hooks report it.
    fn call(app: &mut Desktop<'_>, tool: &str, input: Value) -> Value {
        let (tx, rx) = std::sync::mpsc::channel();
        let request = crate::mcp::request_for_tool(tool, input.clone()).unwrap();
        app.runtime.on_hook(hook("PreToolUse", tool, tool, input.clone(), Value::Null));
        app.runtime.control(serde_json::to_value(request).and_then(serde_json::from_value).unwrap(), tx);
        settle(app);
        let crate::ipc::ControlOutcome::Done(response) =
            rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(2))).unwrap()
        else {
            panic!("deferred")
        };
        assert!(response.ok, "{response:?}");
        app.runtime.on_hook(hook("PostToolUse", tool, tool, input, json!({"structuredContent": response.result})));
        response.result
    }

    fn fake_profile(app: &mut Desktop<'_>, dir: &Path) {
        let state = dir.join("fake.json");
        let config = example_profile("meaning-example", &state);
        app.runtime.config.tickets = config;
        app.runtime.apply_ticket_config();
    }

    fn chief_texts(
        app: &mut Desktop<'_>,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
    ) -> (egui::FullOutput, Vec<(Pos2, String)>) {
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(900.0, 700.0))),
                events,
                ..Default::default()
            },
            |ui| {
                app.shortcuts(ui.ctx());
                app.chief_content(ui);
            },
        );
        output.textures_delta.clear();
        app.runtime.drain_actions();
        let texts = output
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::epaint::Shape::Text(t) => Some((t.pos, t.galley.text().to_string())),
                _ => None,
            })
            .collect();
        (output, texts)
    }

    fn scripted_chief(app: &mut Desktop<'_>, dir: &Path) {
        fake_profile(app, dir);
        let root = app.runtime.launch_dir.clone();
        let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &root);
        chief.chief = true;
        app.runtime.sessions = vec![chief];
        app.runtime.chief_open = true;
        app.runtime.store.chief_dock = Some(ChiefDock::Floating);
    }

    #[test]
    fn trace_lane_shows_search_read_children_related_and_plan_cards_in_order() {
        with_app(
            "app::trace_lane::tests::trace_lane_shows_search_read_children_related_and_plan_cards_in_order",
            |app, ctx| {
                let dir = crate::test_support::tempdir().unwrap();
                scripted_chief(app, dir.path());
                let found = call(
                    app,
                    "atomics_ticket_search",
                    json!({"query":"personal-agent ticket workspace", "semantic":true}),
                );
                let epic = found["cards"][0]["key"].as_str().unwrap().to_string();
                call(app, "atomics_ticket_read", json!({"ticket":"DEMO-1"}));
                call(app, "atomics_ticket_children", json!({"ticket":"DEMO-1"}));
                call(app, "atomics_ticket_related", json!({"ticket":"DEMO-1"}));
                let plan = json!({"goal":"Epic", "plan":"epic", "tasks":[
                {"id":"t1", "title":"First", "ticket":{"id":"x", "key":"DEMO-2"}},
                {"id":"t2", "title":"Second", "depends_on":["t1"]}]});
                // The graph is on screen at Pre time, before the reply.
                app.runtime.on_hook(hook("PreToolUse", "atomics_plan_set", "plan-1", plan.clone(), Value::Null));
                let running = app.lane_entries().last().map(|e| card_view(e)).unwrap();
                assert_eq!((running.glyph, running.meta.as_str()), (Glyph::Running, "2 tasks"));
                app.runtime.on_hook(hook(
                    "PostToolUse",
                    "atomics_plan_set",
                    "plan-1",
                    plan,
                    json!({"structuredContent":{"plan":"epic", "tasks":2}}),
                ));

                let views: Vec<CardView> = app.lane_entries().into_iter().map(card_view).collect();
                let kinds: Vec<&str> = views.iter().map(|v| v.kind).collect();
                assert_eq!(kinds, ["search", "read", "children", "related", "plan"], "{views:#?}");
                assert!(views.iter().all(|v| v.glyph == Glyph::Ok));
                assert_eq!(views[0].title, "search by meaning");
                assert_eq!(views[1].title, "read DEMO-1");
                assert_eq!(views[2].meta, "4 tickets");
                assert_eq!(views[0].open, Some(0));
                assert_eq!(views[4].meta, "2 tasks");

                // Scroll the search card into view; the lane otherwise sticks to the newest card.
                app.client.ui.lane.step(&app.runtime.trace_data(), 1);
                for _ in 0..4 {
                    chief_texts(app, ctx, vec![]);
                }
                let (_, texts) = chief_texts(app, ctx, vec![]);
                let all = texts.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>().join("\n");
                for expected in
                    ["TRACE", "search by meaning", "read DEMO-1", "children of DEMO-1", "related to DEMO-1", "5 calls"]
                {
                    assert!(all.contains(expected), "{expected} missing in {all}");
                }
                let at =
                    |needle: &str| texts.iter().find(|(_, t)| t.contains(needle)).map(|(p, _)| p.y).unwrap_or(f32::MAX);
                assert!(
                    at("search by meaning") < at("read DEMO-1") && at("read DEMO-1") < at("children of"),
                    "{texts:?}"
                );
                // The footer echo names the newest card.
                assert!(app.lane_echo().unwrap().starts_with("chief · plan epic"));

                // A click on "open ↗" of the search card reads its first result in the panel.
                let search_y = texts.iter().find(|(_, t)| t.contains("search by meaning")).unwrap().0.y;
                let open = texts
                    .iter()
                    .filter(|(_, t)| t == "open ↗")
                    .min_by(|a, b| (a.0.y - search_y).abs().total_cmp(&(b.0.y - search_y).abs()))
                    .expect("open link")
                    .0;
                let click = open + vec2(4.0, 4.0);
                let button = |pressed| egui::Event::PointerButton {
                    pos: click,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                };
                chief_texts(app, ctx, vec![egui::Event::PointerMoved(click)]);
                chief_texts(app, ctx, vec![button(true)]);
                chief_texts(app, ctx, vec![button(false)]);
                settle(app);
                assert!(app.runtime.panel == PanelMode::Tickets);
                assert_eq!(app.runtime.tickets.panel.read.as_ref().map(|r| r.link.label().to_string()), Some(epic));
            },
        );
    }

    #[test]
    fn trace_lane_steps_cards_with_shift_arrows_when_focused() {
        with_app("app::trace_lane::tests::trace_lane_steps_cards_with_shift_arrows_when_focused", |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            scripted_chief(app, dir.path());
            for tool in ["atomics_ticket_read", "atomics_ticket_children", "atomics_ticket_related"] {
                call(app, tool, json!({"ticket":"DEMO-1"}));
            }
            let seqs: Vec<u64> = app.lane_entries().iter().map(|e| e.seq).collect();
            chief_texts(app, ctx, vec![]);
            ctx.memory_mut(|m| m.request_focus(focus_id()));
            chief_texts(app, ctx, vec![]);
            assert!(ctx.memory(|m| m.has_focus(focus_id())));
            let shift = |key| {
                vec![
                    egui::Event::ModifiersChanged(egui::Modifiers::SHIFT),
                    egui::Event::Key {
                        key,
                        physical_key: Some(key),
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::SHIFT,
                    },
                ]
            };
            chief_texts(app, ctx, shift(egui::Key::ArrowDown));
            assert_eq!(app.client.ui.lane.selected, Some(seqs[0]));
            chief_texts(app, ctx, shift(egui::Key::ArrowDown));
            chief_texts(app, ctx, shift(egui::Key::ArrowDown));
            chief_texts(app, ctx, shift(egui::Key::ArrowDown));
            assert_eq!(app.client.ui.lane.selected, Some(seqs[2]), "clamps at the last card");
            chief_texts(app, ctx, shift(egui::Key::ArrowUp));
            assert_eq!(app.client.ui.lane.selected, Some(seqs[1]));
            // Enter expands the selected card; Escape hands the focus to the terminal.
            let enter = egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: Some(egui::Key::Enter),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            };
            chief_texts(app, ctx, vec![egui::Event::ModifiersChanged(egui::Modifiers::NONE), enter]);
            assert!(app.client.ui.lane.expanded.contains(&seqs[1]));
            let escape = egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: Some(egui::Key::Escape),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            };
            chief_texts(app, ctx, vec![escape]);
            assert!(!ctx.memory(|m| m.has_focus(focus_id())));
        });
    }

    #[test]
    fn trace_lane_clears_and_drops_secrets_never_reach_the_cards() {
        with_app("app::trace_lane::tests::trace_lane_clears_and_drops_secrets_never_reach_the_cards", |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            scripted_chief(app, dir.path());
            app.runtime.on_hook(hook(
                "PostToolUse",
                "other_lookup",
                "x",
                json!({"q":"hello", "api_key":"sk-live-1234567890abcdef"}),
                json!({"error":"unauthorized"}),
            ));
            let (_, texts) = chief_texts(app, ctx, vec![]);
            let all = texts.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>().join("\n");
            assert!(all.contains("atomics: other_lookup") && all.contains("error: unauthorized"), "{all}");
            assert!(!all.contains("sk-live"));
            app.client.ui.lane.cleared = app.lane_entries().last().unwrap().seq;
            assert!(app.lane_entries().is_empty());
            assert_eq!(app.client.ui.lane.height(&app.runtime.trace_data(), 600.0, None), 0.0);
        });
    }
}
