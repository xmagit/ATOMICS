use super::*;

struct BoardFrame {
    text: Vec<egui::epaint::TextShape>,
    cards: Vec<Rect>,
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
            egui::epaint::Shape::Rect(rect) if rect.fill == app.runtime.palette.raised && rect.rect.height() > 40.0 => {
                Some(rect.rect)
            }
            _ => None,
        })
        .collect();
    output.drop_without_applying_deltas();
    BoardFrame { text, cards }
}

fn populate(app: &mut Desktop<'_>) {
    let agent = imacs_core::config::AgentConfig {
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
            isolation: (i == 0).then_some(imacs_core::isolation::IsolationMode::Bare),
            brief: "Detailed instructions stay readable when the card is expanded.".into(),
            acceptance: vec!["Keep the original acceptance checks".into()],
            report: Some(Report {
                status: "progress".into(),
                summary: "Reports keep their ordinary words together.".into(),
                at: 0,
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
            for width in [480.0, 740.0, 1600.0, 5120.0] {
                for i in 0..3 {
                    frame(app, ctx, width, width as f64 + i as f64, vec![]);
                }
                let rendered = frame(app, ctx, width, width as f64 + 3.0, vec![]);
                assert!(!rendered.cards.is_empty());
                for card in &rendered.cards {
                    assert!(card.width() >= 279.0, "{width}: card width {}", card.width());
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
                    "✓ Keep the original acceptance checks",
                    "open",
                ] {
                    assert!(rendered.text.iter().any(|t| t.galley.text().contains(content)), "missing {content}");
                }
                if width == 5120.0 {
                    assert!(rendered.text.iter().any(|t| t.galley.text() == "after task-0"));
                    for label in ["start", "unblock", "accept", "send back", "reopen", "integrate"] {
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
fn board_scrolls_horizontally_to_the_last_column_and_keeps_card_buttons() {
    super::tests::with_app(
        "app::board_tests::board_scrolls_horizontally_to_the_last_column_and_keeps_card_buttons",
        |app, ctx| {
            populate(app);
            for i in 0..3 {
                frame(app, ctx, 740.0, i as f64, vec![]);
            }
            let rendered = frame(app, ctx, 740.0, 3.0, vec![]);
            let first = rendered.text.iter().find(|t| t.galley.text().starts_with("Readable 0 ")).unwrap();
            let original_x = first.pos.x;
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
                .expect("last status column must be reachable");
            assert!(last.pos.x > 0.0 && last.pos.x < 740.0);
            assert!(!rendered.text.iter().any(|t| t.galley.text().starts_with("Readable 0 ") && t.pos.x == original_x));
            for label in ["reopen", "integrate"] {
                assert!(rendered.text.iter().any(|t| t.galley.text() == label), "missing {label} button");
            }
            let reopen = rendered.text.iter().find(|t| t.galley.text() == "reopen").unwrap();
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
        },
    );
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
                        ["start", "accept", "send back", "unblock", "reopen", "integrate", "resume", "Approve plan"]
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
fn approve_plan_fills_with_the_accent_and_writes_on_accent() {
    super::tests::with_app("app::board_tests::approve_plan_fills_with_the_accent_and_writes_on_accent", |app, ctx| {
        populate(app);
        app.runtime.plans[0].approved = false;
        let p = app.runtime.palette.clone();
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
