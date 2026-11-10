use super::*;

#[test]
fn accepted_tasks_show_the_integration_count_and_action() {
    tests::with_app("app::board_tests::accepted_tasks_show_the_integration_count_and_action", |app, ctx| {
        app.runtime.plans[0].tasks = vec![Task {
            id: "a".into(),
            title: "Accepted work".into(),
            status: TaskStatus::Done,
            branch: Some("feature".into()),
            ..Default::default()
        }];
        let rendered = frame(app, ctx, 1400.0, 0.0, vec![]);
        let labels: Vec<_> = rendered.text.iter().map(|text| text.galley.text()).collect();
        assert!(labels.contains(&"1 accepted task not integrated yet"), "{labels:?}");
        assert!(labels.contains(&"Integrate"));
    });
}

struct BoardFrame {
    text: Vec<egui::epaint::TextShape>,
    cards: Vec<Rect>,
    /// The addresses it opened in the browser.
    opened: Vec<String>,
}

#[test]
fn review_reason_is_visible_and_check_evidence_expands_on_the_card() {
    tests::with_app("app::board_tests::review_reason_is_visible_and_check_evidence_expands_on_the_card", |app, ctx| {
        app.runtime.plans[0].tasks = vec![Task {
            id: "review".into(),
            title: "Final review".into(),
            agent: "codex".into(),
            reviewer_reason: Some("Selected by [plan] review_agent override.".into()),
            report: Some(Report {
                status: "done".into(),
                summary: "Checked".into(),
                at: 1,
                checks: vec![plan::Check {
                    check: "Acceptance check".into(),
                    command: "cargo test".into(),
                    exit: Some(0),
                    output_tail: "test result: ok".into(),
                }],
            }),
            ..Task::default()
        }];
        frame(app, ctx, 1600.0, 0.0, vec![]);
        let rendered = frame(app, ctx, 1600.0, 0.5, vec![]);
        let labels: Vec<_> = rendered.text.iter().map(|text| text.galley.text()).collect();
        assert!(labels.contains(&"Selected by [plan] review_agent override."));
        assert!(!labels.contains(&"cargo test"), "evidence starts collapsed");
        let heading = rendered.text.iter().find(|text| text.galley.text() == "Checks (1)").expect("checks expander");
        let pos = heading.visual_bounding_rect().center();
        for (time, pressed) in [(1.0, true), (1.1, false)] {
            frame(
                app,
                ctx,
                1600.0,
                time,
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
        let rendered = frame(app, ctx, 1600.0, 2.0, vec![]);
        let labels: Vec<_> = rendered.text.iter().map(|text| text.galley.text()).collect();
        for expected in ["Acceptance check", "cargo test", "exit 0", "test result: ok"] {
            assert!(labels.contains(&expected), "{expected}: {labels:?}");
        }
    });
}

fn frame(app: &mut Desktop<'_>, ctx: &egui::Context, width: f32, time: f64, events: Vec<egui::Event>) -> BoardFrame {
    let output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(width, 1000.0))),
            time: Some(time),
            events,
            ..Default::default()
        },
        |ui| {
            egui::CentralPanel::default().show(ui, |ui| app.plan_panel(ui, ui.max_rect()));
        },
    );
    app.runtime.drain_actions();
    let text = output
        .shapes
        .iter()
        .filter_map(|shape| match &shape.shape {
            egui::epaint::Shape::Text(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    let cards = output
        .shapes
        .iter()
        .filter_map(|shape| match &shape.shape {
            egui::epaint::Shape::Rect(rect) if rect.fill == app.palette().raised && rect.rect.height() > 40.0 => {
                Some(rect.rect)
            }
            _ => None,
        })
        .collect();
    let opened = output
        .platform_output
        .commands
        .iter()
        .filter_map(|command| match command {
            egui::OutputCommand::OpenUrl(open) => Some(open.url.clone()),
            _ => None,
        })
        .collect();
    output.drop_without_applying_deltas();
    BoardFrame { text, cards, opened }
}

fn populate(app: &mut Desktop<'_>) {
    let agent = atomics_core::config::AgentConfig {
        name: "codex".into(),
        command: vec![],
        kind: AgentKind::Generic,
        busy_text: None,
        isolation: None,
    };
    app.runtime.sessions = vec![Session::starting(
        "existing-session".into(),
        &agent,
        app.runtime.launch_dir.clone(),
        false,
        &app.runtime.wake,
    )];
    let tasks = TaskStatus::ALL
        .into_iter()
        .enumerate()
        .map(|(i, status)| Task {
            id: format!("task-{i}"),
            title: format!("Readable {i} plan cards keep ordinary words together across several lines"),
            branch: Some(format!("issue/1234-{}", "a-long-branch-name-".repeat(10))),
            status,
            depends_on: if i == 0 { vec![] } else { vec!["task-0".into()] },
            session: Some("existing-session".into()),
            agent: "codex".into(),
            args: vec!["-m".into(), "gpt-6.1-terra".into()],
            // A task the chief marked bare says so on its card.
            isolation: (i == 0).then_some(atomics_core::isolation::IsolationMode::Bare),
            brief: "Detailed instructions stay readable when the card is expanded.".into(),
            acceptance: vec!["Keep the original acceptance checks".into()],
            report: Some(Report {
                status: "progress".into(),
                summary: "Reports keep their ordinary words together.".into(),
                at: 0,
                checks: Vec::new(),
            }),
            notes: vec!["Keep the original task note".into()],
            ..Task::default()
        })
        .collect();
    app.runtime.plans = vec![Plan {
        id: "readable-board".into(),
        goal: "Readable plan board cards".into(),
        tasks,
        approved: true,
        ..Plan::default()
    }];
    app.runtime.selected_plan = 0;
    for task in &app.runtime.plans[0].tasks {
        app.client.expanded_tasks.insert((app.runtime.plans[0].id.clone(), task.id.clone()));
    }
}

#[test]
fn cards_wrap_words_with_normal_spacing_at_narrow_and_wide_sizes() {
    super::tests::with_app(
        "app::board_tests::cards_wrap_words_with_normal_spacing_at_narrow_and_wide_sizes",
        |app, ctx| {
            populate(app);
            // Narrow boards keep 240-point columns; wider boards have room to expand them.
            for (width, min_card_width) in [(480.0, 239.0), (740.0, 239.0), (1600.0, 279.0), (5120.0, 279.0)] {
                for i in 0..3 {
                    frame(app, ctx, width, width as f64 + i as f64, vec![]);
                }
                let rendered = frame(app, ctx, width, width as f64 + 3.0, vec![]);
                assert!(!rendered.cards.is_empty());
                for card in &rendered.cards {
                    assert!(card.width() >= min_card_width, "{width}: card width {}", card.width());
                }
                let titles: Vec<_> =
                    rendered.text.iter().filter(|t| t.galley.text().starts_with("Readable ")).collect();
                assert!(!titles.is_empty());
                for text in rendered.text.iter().filter(|text| rendered.cards.iter().any(|r| r.contains(text.pos))) {
                    assert!(!text.galley.job.justify, "card text must not spread across justified rows");
                    for row in &text.galley.rows {
                        for pair in row.glyphs.windows(2) {
                            let gap = pair[1].pos.x - pair[0].pos.x - pair[0].advance_width;
                            assert!(gap <= 1.0, "{width}: expanded glyph spacing in {:?}", text.galley.text());
                        }
                    }
                }
                for title in titles {
                    let mut consumed = String::new();
                    for row in &title.galley.rows {
                        let text: String = row.glyphs.iter().map(|g| g.chr).collect();
                        assert!(text.trim().len() > 1, "{width}: single-character row {text:?}");
                        consumed.push_str(&text);
                        let end = consumed.len();
                        let original = title.galley.text();
                        assert!(
                            end == original.len() || original[..end].ends_with(' ') || original[end..].starts_with(' '),
                            "{width}: split word at {consumed:?}"
                        );
                    }
                }
                for content in [
                    "· codex gpt-6.1-terra",
                    "task-0 · codex gpt-6.1-terra · bare",
                    "progress:",
                    "Keep the original task note",
                    "Detailed instructions",
                    "Keep the original acceptance checks",
                    "Open",
                ] {
                    assert!(rendered.text.iter().any(|t| t.galley.text().contains(content)), "missing {content}");
                }
                if width == 5120.0 {
                    assert!(rendered.text.iter().any(|t| t.galley.text() == "after task-0"));
                    for label in ["Start", "Unblock", "Accept", "Send back", "Reopen", "Integrate"] {
                        assert!(rendered.text.iter().any(|t| t.galley.text() == label), "missing {label} button");
                    }
                }
            }
        },
    );
}

#[test]
fn branch_names_are_single_line_elided_and_show_the_full_name_on_hover() {
    super::tests::with_app(
        "app::board_tests::branch_names_are_single_line_elided_and_show_the_full_name_on_hover",
        |app, ctx| {
            populate(app);
            for i in 0..3 {
                frame(app, ctx, 1600.0, i as f64, vec![]);
            }
            let rendered = frame(app, ctx, 1600.0, 3.0, vec![]);
            let branch = rendered.text.iter().find(|t| t.galley.text().starts_with("issue/1234-")).unwrap();
            assert!(branch.galley.elided);
            assert_eq!(branch.galley.rows.len(), 1);
            assert_eq!(branch.galley.rows[0].glyphs.last().unwrap().chr, '…');
            let full = app.runtime.plans[0].tasks[0].branch.clone().unwrap();
            let hover = branch.pos + branch.galley.rect.center().to_vec2();
            frame(app, ctx, 1600.0, 4.0, vec![egui::Event::PointerMoved(hover)]);
            frame(app, ctx, 1600.0, 5.0, vec![]);
            let rendered = frame(app, ctx, 1600.0, 6.0, vec![]);
            assert!(
                rendered.text.iter().any(|t| t.galley.text() == full && !t.galley.elided),
                "hover must expose the full branch name"
            );
        },
    );
}

#[test]
fn a_narrow_board_scrolls_sideways_and_keeps_card_buttons() {
    super::tests::with_app("app::board_tests::a_narrow_board_scrolls_sideways_and_keeps_card_buttons", |app, ctx| {
        populate(app);
        for i in 0..3 {
            frame(app, ctx, 740.0, i as f64, vec![]);
        }
        let rendered = frame(app, ctx, 740.0, 3.0, vec![]);
        let first = rendered.text.iter().find(|t| t.galley.text().starts_with("Readable 0 ")).unwrap();
        // Too narrow for five columns: they keep their width and sit side by side in a horizontal scroll.
        for card in &rendered.cards {
            assert!((card.width() - 240.0).abs() < 1.0, "card width {}", card.width());
        }
        assert!(first.pos.x < 60.0, "the first card starts at the panel's left: {:?}", first.pos);
        let second = rendered.text.iter().find(|t| t.galley.text().starts_with("Readable 1 ")).unwrap();
        assert!(second.pos.x >= first.pos.x + 240.0, "the columns keep their width");
        assert!((second.pos.y - first.pos.y).abs() < 1.0, "the statuses share a row");
        let pointer = first.pos + vec2(30.0, 8.0);
        frame(
            app,
            ctx,
            740.0,
            4.0,
            vec![
                egui::Event::PointerMoved(pointer),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    phase: egui::TouchPhase::Move,
                    source: egui::MouseWheelSource::Unknown,
                    delta: vec2(-2000.0, 0.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        for time in 5..10 {
            frame(app, ctx, 740.0, time as f64, vec![]);
        }
        let rendered = frame(app, ctx, 740.0, 10.0, vec![]);
        let last = rendered
            .text
            .iter()
            .find(|t| t.galley.text().starts_with("Readable 4 "))
            .expect("the last status must be reachable");
        assert!(last.pos.x > 0.0 && last.pos.x < 740.0, "{:?}", last.pos);
        assert!((last.pos.y - first.pos.y).abs() < 1.0, "scrolling keeps the statuses in their row");
        let card = rendered.cards.iter().find(|card| card.contains(last.pos)).expect("the last card is visible");
        assert!(card.left() >= 0.0 && card.right() <= 740.0, "the last card fits in the panel: {card:?}");
        for label in ["Reopen", "Integrate"] {
            assert!(rendered.text.iter().any(|t| t.galley.text() == label), "missing {label} button");
        }
        let reopen = rendered.text.iter().find(|t| t.galley.text() == "Reopen").unwrap();
        let pos = reopen.pos + reopen.galley.rect.center().to_vec2();
        frame(app, ctx, 740.0, 11.0, vec![egui::Event::PointerMoved(pos)]);
        for (time, pressed) in [(12.0, true), (12.05, false)] {
            frame(
                app,
                ctx,
                740.0,
                time,
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
        assert_eq!(app.runtime.plans[0].tasks[4].status, TaskStatus::Review);
    });
}

/// A plan and its task, each following a ticket of `system`, the plan's without an address.
fn ticketed(app: &mut Desktop<'_>, system: &str) {
    use atomics_core::tickets::TicketLink;
    let link = |id: &str, url: Option<&str>| TicketLink {
        system: system.into(),
        id: id.into(),
        url: url.map(str::to_string),
        ..TicketLink::default()
    };
    app.runtime.plans = vec![Plan {
        id: "ticketed".into(),
        goal: "A plan with tickets".into(),
        ticket: Some(link("PLAN-1", None)),
        tasks: vec![Task {
            id: "t".into(),
            title: "A task with a ticket".into(),
            ticket: Some(link("TASK-1", Some("https://tickets.example/TASK-1"))),
            ..Task::default()
        }],
        ..Plan::default()
    }];
    app.runtime.selected_plan = 0;
}

/// A click on the chip that says `label`: what it opened in the browser.
fn click_chip(app: &mut Desktop<'_>, ctx: &egui::Context, label: &str, time: f64) -> Vec<String> {
    frame(app, ctx, 1600.0, time, vec![]);
    let rendered = frame(app, ctx, 1600.0, time + 0.1, vec![]);
    let chip = rendered.text.iter().find(|t| t.galley.text() == label).unwrap();
    let pos = chip.pos + chip.galley.rect.center().to_vec2();
    let press = |pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    frame(app, ctx, 1600.0, time + 0.2, vec![egui::Event::PointerMoved(pos), press(true)]);
    frame(app, ctx, 1600.0, time + 0.25, vec![press(false)]).opened
}

#[test]
fn a_ticket_chip_without_a_reader_for_it_opens_its_address() {
    let name = "app::board_tests::a_ticket_chip_without_a_reader_for_it_opens_its_address";
    super::tests::with_app(name, |app, ctx| {
        let address = ["https://tickets.example/TASK-1"];
        // No ticket system: there is nothing to read it from.
        ticketed(app, "demo");
        app.runtime.tickets.config = None;
        assert_eq!(click_chip(app, ctx, "TASK-1", 1.0), address);
        assert!(click_chip(app, ctx, "PLAN-1", 2.0).is_empty(), "no address, nothing to open");
        assert_ne!(app.runtime.panel, PanelMode::Tickets);
        // Another system's ticket: the configured system's reader would look up another ticket.
        app.runtime.tickets.config =
            Some(Arc::new(atomics_core::tickets::TicketConfig { system: "other".into(), ..Default::default() }));
        assert_eq!(click_chip(app, ctx, "TASK-1", 3.0), address);
        assert_ne!(app.runtime.panel, PanelMode::Tickets);
        assert!(app.runtime.tickets.panel.read.is_none());
        for (index, url) in ["file:///tmp/test", "javascript:alert(1)", "data:text/html,test", "httpx://example.com"]
            .into_iter()
            .enumerate()
        {
            app.runtime.plans[0].tasks[0].ticket.as_mut().unwrap().url = Some(url.into());
            assert!(click_chip(app, ctx, "TASK-1", 4.0 + index as f64).is_empty(), "{url}");
        }
    });
}

#[test]
fn a_ticket_chip_says_what_a_click_does() {
    super::tests::with_app("app::board_tests::a_ticket_chip_says_what_a_click_does", |app, ctx| {
        let hint = |app: &mut Desktop<'_>, ctx: &egui::Context, label: &str, time: f64| {
            let rendered = frame(app, ctx, 1600.0, time, vec![]);
            let chip = rendered.text.iter().find(|t| t.galley.text() == label).unwrap();
            let pos = chip.pos + chip.galley.rect.center().to_vec2();
            frame(app, ctx, 1600.0, time + 1.0, vec![egui::Event::PointerMoved(pos)]);
            frame(app, ctx, 1600.0, time + 2.0, vec![]);
            let rendered = frame(app, ctx, 1600.0, time + 3.0, vec![]);
            let says = |t: &&egui::epaint::TextShape| t.galley.text().starts_with("click to");
            rendered.text.iter().find(says).map(|t| t.galley.text().to_string())
        };
        ticketed(app, "demo");
        app.runtime.tickets.config = None;
        let opens = "click to open https://tickets.example/TASK-1";
        assert_eq!(hint(app, ctx, "TASK-1", 1.0).as_deref(), Some(opens));
        assert_eq!(hint(app, ctx, "PLAN-1", 5.0), None);
        app.runtime.tickets.config =
            Some(Arc::new(atomics_core::tickets::TicketConfig { system: "demo".into(), ..Default::default() }));
        let reads = "click to read it here, with Open in browser";
        assert_eq!(hint(app, ctx, "TASK-1", 9.0).as_deref(), Some(reads));
        assert_eq!(hint(app, ctx, "PLAN-1", 13.0).as_deref(), Some("click to read it here"));
    });
}

#[test]
fn completed_and_archived_boards_are_read_only_even_with_ready_tasks() {
    super::tests::with_app(
        "app::board_tests::completed_and_archived_boards_are_read_only_even_with_ready_tasks",
        |app, ctx| {
            populate(app);
            let dir = crate::test_support::tempdir().unwrap();
            app.runtime.plans[0].tasks[4].worktree = Some(dir.path().into());
            app.runtime.plans[0].tasks[4].finished_at = Some(890);
            app.runtime.plans[0].tasks[4].resume_id = Some("resume-id".into());
            app.runtime.plans[0].tasks[4].session = None;
            for status in [PlanStatus::Completed, PlanStatus::Archived] {
                for approved in [true, false] {
                    app.runtime.plans[0].status = status;
                    app.runtime.plans[0].approved = approved;
                    let before = app.runtime.plans[0].clone();
                    let rendered = frame(app, ctx, 5120.0, 10.0, vec![]);
                    let labels: Vec<_> = rendered.text.iter().map(|text| text.galley.text()).collect();
                    assert!(labels.contains(&"Reopen plan"));
                    assert!(labels.iter().any(|text| text.contains("read-only")));
                    for forbidden in
                        ["Start", "Accept", "Send back", "Unblock", "Reopen", "Integrate", "Resume", "Approve plan"]
                    {
                        assert!(!labels.contains(&forbidden), "{status:?} offers {forbidden}: {labels:?}");
                    }
                    assert!(!labels.iter().any(|text| text.starts_with("Dispatch ready")));
                    assert_eq!(app.runtime.plans[0], before);
                    assert!(app.runtime.creating.is_empty());
                    assert!(app.runtime.dispatch(0, Some("task-0")).unwrap_err().contains("Reopen"));
                    app.runtime.task_action(0, 4, "resume");
                    assert_eq!(app.runtime.plans[0], before);
                }
            }
        },
    );
}

#[test]
fn send_fills_with_the_accent_only_with_something_to_send() {
    super::tests::with_app("app::board_tests::send_fills_with_the_accent_only_with_something_to_send", |app, ctx| {
        populate(app);
        let accent = app.palette().accent;
        let filled = |app: &mut Desktop<'_>| {
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1600.0, 1000.0))),
                    ..Default::default()
                },
                |ui| app.paint(ui),
            );
            app.runtime.drain_actions();
            let label = output.shapes.iter().find_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(text) if text.galley.text() == "Send" => {
                    Some(text.visual_bounding_rect().center())
                }
                _ => None,
            });
            let label = label.expect("the steer bar's Send");
            let filled = output.shapes.iter().any(|shape| {
                matches!(&shape.shape, egui::epaint::Shape::Rect(rect)
                    if rect.fill == accent && rect.rect.contains(label))
            });
            output.drop_without_applying_deltas();
            filled
        };
        filled(app);
        assert!(!filled(app), "nothing typed, nothing to send");
        app.client.ui.steer_text = "   ".into();
        assert!(!filled(app), "spaces send nothing either");
        app.client.ui.steer_text = "go on".into();
        assert!(filled(app));
    });
}

#[test]
fn approve_plan_fills_with_the_accent_and_writes_on_accent() {
    super::tests::with_app("app::board_tests::approve_plan_fills_with_the_accent_and_writes_on_accent", |app, ctx| {
        populate(app);
        app.runtime.plans[0].approved = false;
        let p = app.palette().clone();
        assert_ne!(p.on_accent, p.foreground, "the check needs on_accent to differ from ordinary text");
        for time in 0..2 {
            frame(app, ctx, 1600.0, time as f64, vec![]);
        }
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1600.0, 1000.0))),
                time: Some(2.0),
                ..Default::default()
            },
            |ui| {
                egui::CentralPanel::default().show(ui, |ui| app.plan_panel(ui, ui.max_rect()));
            },
        );
        app.runtime.drain_actions();
        let label = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(text) if text.galley.text() == "Approve plan" => Some(text.clone()),
                _ => None,
            })
            .expect("an unapproved plan shows Approve plan");
        assert!(label.galley.job.sections.iter().all(|section| section.format.color == p.on_accent));
        let filled = output.shapes.iter().any(|shape| {
            matches!(&shape.shape, egui::epaint::Shape::Rect(rect)
                if rect.fill == p.accent && rect.rect.contains(label.visual_bounding_rect().center()))
        });
        assert!(filled, "the button behind the label is filled with the accent");
        output.drop_without_applying_deltas();
    });
}

#[test]
fn a_card_says_how_many_notes_it_hides_and_shows_them_all_when_open() {
    super::tests::with_app(
        "app::board_tests::a_card_says_how_many_notes_it_hides_and_shows_them_all_when_open",
        |app, ctx| {
            populate(app);
            let plan = app.runtime.selected_plan;
            app.runtime.plans[plan].tasks[0].notes =
                vec!["The first note".into(), "The second note".into(), "The last note".into()];
            let key = (app.runtime.plans[plan].id.clone(), "task-0".to_string());
            app.client.ui.expanded_tasks.remove(&key);
            let texts = |app: &mut Desktop<'_>, time: f64| -> Vec<String> {
                frame(app, ctx, 1600.0, time, vec![]);
                frame(app, ctx, 1600.0, time + 0.5, vec![]).text.iter().map(|t| t.galley.text().to_string()).collect()
            };
            let shown = texts(app, 0.0);
            assert!(shown.contains(&"The last note".into()), "{shown:?}");
            assert!(!shown.contains(&"The first note".into()), "a closed card shows the last note");
            assert!(shown.contains(&"2 earlier notes · click the title".into()), "{shown:?}");
            app.client.ui.expanded_tasks.insert(key);
            let shown = texts(app, 2.0);
            for note in ["The first note", "The second note", "The last note"] {
                assert!(shown.contains(&note.into()), "an open card shows {note}: {shown:?}");
            }
            assert!(!shown.iter().any(|t| t.contains("earlier note")));
        },
    );
}

#[test]
fn a_refused_sign_in_shows_reconnect_on_the_board_and_a_click_signs_in_again() {
    tests::with_app(
        "app::board_tests::a_refused_sign_in_shows_reconnect_on_the_board_and_a_click_signs_in_again",
        |app, ctx| {
            let fake = crate::mcp_client::FakeOAuthServer::start();
            let (_dir, oauth) = crate::mcp_client::oauth_test_machine();
            app.runtime.config.tickets = toml::from_str("system = \"tracker\"").unwrap();
            app.runtime.config.tickets.server = fake.server();
            app.runtime.tickets.sign.override_oauth(oauth);
            app.runtime.tickets.sign.headless = true;
            app.runtime.apply_ticket_config();
            app.runtime.poll_signin();
            app.runtime.plans[0].tasks = vec![Task { id: "a".into(), title: "Work".into(), ..Default::default() }];
            let labels = |rendered: &BoardFrame| -> Vec<String> {
                rendered.text.iter().map(|text| text.galley.text().to_string()).collect()
            };
            // Never signed in here: the board says so, with the button that signs in.
            let rendered = frame(app, ctx, 1400.0, 0.0, vec![]);
            assert!(labels(&rendered).contains(&"Sign in to tracker".to_string()), "{:?}", labels(&rendered));

            // The server refused the saved sign-in: the same place offers Reconnect, and says why.
            app.runtime.ticket_job_failed("sign-in required again: the server refused the saved sign-in");
            app.runtime.poll_signin();
            let rendered = frame(app, ctx, 1400.0, 0.1, vec![]);
            let texts = labels(&rendered);
            assert!(texts.contains(&"tracker needs you to sign in again".to_string()), "{texts:?}");
            assert!(texts.contains(&"the server refused the saved sign-in".to_string()), "{texts:?}");
            let button = rendered.text.iter().find(|text| text.galley.text() == "Reconnect").expect("Reconnect");
            let at = button.pos + button.galley.rect.center().to_vec2();
            let press = |pressed| {
                vec![
                    egui::Event::PointerMoved(at),
                    egui::Event::PointerButton {
                        pos: at,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: Default::default(),
                    },
                ]
            };
            frame(app, ctx, 1400.0, 0.2, press(true));
            frame(app, ctx, 1400.0, 0.3, press(false));
            assert!(app.runtime.tickets.sign.job.is_some(), "the click started the sign-in");
            // With no browser here, the board shows the steps: the link to open and the paste box.
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(20));
            while app.runtime.sign_in_info().and_then(|info| info.url).is_none() {
                app.runtime.poll_signin();
                assert!(Instant::now() < deadline, "no sign-in address");
                std::thread::sleep(Duration::from_millis(5));
            }
            let texts = labels(&frame(app, ctx, 1400.0, 0.4, vec![]));
            assert!(texts.contains(&"Waiting for the sign-in…".to_string()), "{texts:?}");
            assert!(texts.contains(&"Open link".to_string()) && texts.contains(&"Use it".to_string()), "{texts:?}");
            let _ = app.runtime.cancel_sign_in();
        },
    );
}

#[test]
fn notebook_shows_proposals_and_authors_and_user_can_pin_and_delete() {
    tests::with_app(
        "app::board_tests::notebook_shows_proposals_and_authors_and_user_can_pin_and_delete",
        |app, ctx| {
            app.runtime.config.plan.notebook = true;
            app.runtime.plans[0].id = "notebook-board".into();
            app.runtime.plans[0]
                .propose_note("task-a", "Agent", atomics_core::store::now(), "Build in the Nix shell")
                .unwrap();
            let rendered = frame(app, ctx, 1600.0, 0.0, vec![]);
            let labels: Vec<_> = rendered.text.iter().map(|t| t.galley.text()).collect();
            assert!(labels.iter().any(|s| s.contains("0 pinned · 1 proposals")), "{labels:?}");
            assert!(labels.iter().any(|s| s.contains("task-a · Agent") && s.contains("s ago")), "{labels:?}");
            assert!(labels.contains(&"Build in the Nix shell"));
            click_chip(app, ctx, "Pin", 1.0);
            assert!(app.runtime.plans[0].notebook[0].pinned);
            click_chip(app, ctx, "Delete", 2.0);
            assert!(app.runtime.plans[0].notebook.is_empty());
            assert!(Plan::load(&paths::atomics_home().join("plans/notebook-board.json")).unwrap().notebook.is_empty());
        },
    );
}

#[test]
fn github_agent_report_records_pull_request_and_shows_it_on_task_card() {
    tests::with_app(
        "app::board_tests::github_agent_report_records_pull_request_and_shows_it_on_task_card",
        |app, ctx| {
            app.runtime.plans[0].tasks = vec![Task {
                id: "work".into(),
                title: "Work".into(),
                session: Some("task-session".into()),
                branch: Some("task".into()),
                status: TaskStatus::Running,
                ..Default::default()
            }];
            let request: ControlRequest = serde_json::from_value(json!({
                "cmd":"report", "pane":"task-session", "status":"done", "summary":"Opened the requested PR",
                "pull_request":{"url":"https://github.com/group/repo/pull/12","number":12,"head":"task","base":"main"}
            }))
            .unwrap();
            let (tx, rx) = channel();
            app.runtime.control(request.clone(), tx);
            let crate::ipc::ControlOutcome::Done(response) = rx.recv().unwrap() else { panic!("report outcome") };
            assert!(response.ok);
            let task = &app.runtime.plans[0].tasks[0];
            assert_eq!(task.status, TaskStatus::Review);
            assert_eq!(task.pull_request.as_ref().unwrap().number, 12);
            assert!(
                task.stages.iter().any(
                    |s| s.stage == "pr_opened" && s.url.as_deref() == Some("https://github.com/group/repo/pull/12")
                )
            );
            let count = task.stages.len();
            let (tx, _rx) = channel();
            app.runtime.control(request, tx);
            assert_eq!(app.runtime.plans[0].tasks[0].stages.len(), count, "duplicate report is idempotent");
            frame(app, ctx, 1400.0, 0.0, vec![]);
            let rendered = frame(app, ctx, 1400.0, 0.5, vec![]);
            assert!(rendered.text.iter().any(|t| t.galley.text() == "PR #12 · task → main"));
            assert_eq!(app.runtime.task_view(0, "work").unwrap()["task"]["pull_request"]["number"], 12);
        },
    );
}

#[test]
fn github_publish_card_shows_full_request_and_dismiss_click_removes_it() {
    tests::with_app(
        "app::board_tests::github_publish_card_shows_full_request_and_dismiss_click_removes_it",
        |app, ctx| {
            app.runtime.plans[0].approved = true;
            app.runtime.plans[0].publish = Some(plan::Publish {
                id: "publish-request".into(),
                repository: "group/repo".into(),
                account: "account-two".into(),
                branch: "task".into(),
                base: "main".into(),
                head_sha: "1".repeat(40),
                base_sha: "0".repeat(40),
                title: "Reviewed changes".into(),
                body: "Full description\nSecond paragraph with all details.".into(),
                commits: "1234567 Contributor <contributor@example.invalid> Reviewed changes".into(),
                identity_digest: String::new(),
                url: None,
                error: None,
            });
            frame(app, ctx, 1400.0, 0.0, vec![]);
            let rendered = frame(app, ctx, 1400.0, 0.5, vec![]);
            for expected in [
                "group/repo · GitHub account: account-two",
                "Reviewed changes",
                "Full description\nSecond paragraph with all details.",
                "1234567 Contributor <contributor@example.invalid> Reviewed changes",
                "Publish",
                "Dismiss",
            ] {
                assert!(rendered.text.iter().any(|t| t.galley.text() == expected), "missing {expected}");
            }
            let button =
                rendered.text.iter().find(|t| t.galley.text() == "Dismiss").unwrap().visual_bounding_rect().center();
            for (time, pressed) in [(1.0, true), (1.1, false)] {
                frame(
                    app,
                    ctx,
                    1400.0,
                    time,
                    vec![
                        egui::Event::PointerMoved(button),
                        egui::Event::PointerButton {
                            pos: button,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
            assert!(app.runtime.plans[0].publish.is_none());
        },
    );
}
