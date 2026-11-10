//! The fleet's host side: which sessions and finished tasks the current tab shows, closing idle sessions,
//! resuming an exited one, the slot-queue note, and the desktop's terminal surface. atomics-ui draws the
//! tiles and the tab strip (`atomics_ui::fleet`).
use atomics_ui::terminal_surface::TerminalSurface;

use super::*;

impl State {
    /// Sessions in the selected project or plan view, excluding the chief.
    pub(super) fn visible_sessions(&self) -> Vec<usize> {
        (0..self.sessions.len())
            .filter(|&i| !self.sessions[i].chief)
            .filter(|&i| {
                if self.plan_view {
                    self.plans[self.selected_plan].contains_session(&self.sessions[i].id)
                } else if self.ungrouped {
                    self.ungrouped_session(&self.sessions[i])
                } else {
                    match &self.view {
                        None => true,
                        Some(project) => self.project_of(&self.sessions[i].root()).as_ref() == Some(project),
                    }
                }
            })
            .collect()
    }

    pub(super) fn completed_tasks(&self) -> Vec<(usize, usize)> {
        // Completed tasks belong to plans, so the Ungrouped tab shows none.
        if self.ungrouped && !self.plan_view {
            return Vec::new();
        }
        self.plans
            .iter()
            .enumerate()
            .filter(|(index, plan)| {
                plan.status != plan::PlanStatus::Archived
                    && if self.plan_view {
                        *index == self.selected_plan
                    } else {
                        match &self.view {
                            None => plan.is_open(),
                            Some(project) => plan.project.as_ref() == Some(project),
                        }
                    }
            })
            .flat_map(|(index, plan)| {
                plan.tasks
                    .iter()
                    .enumerate()
                    .filter(|(_, task)| {
                        task.finished_at.is_some()
                            && task.worktree.is_some()
                            && !self.sessions.iter().any(|s| task.session.as_deref() == Some(&s.id))
                    })
                    .map(move |(task, _)| (index, task))
            })
            .collect()
    }

    /// Closes the project's idle and exited agents that no plan task is using. Shells
    /// are always idle to ATOMICS, so they are left alone.
    pub(super) fn close_idle(&mut self, project: &Path) -> usize {
        let in_plan: HashSet<String> =
            self.plans.iter().flat_map(|p| &p.tasks).filter_map(|t| t.session.clone()).collect();
        let idle: Vec<String> = self
            .sessions
            .iter()
            .filter(|s| !s.chief && s.is_agent() && matches!(s.status, AgentStatus::Idle | AgentStatus::Exited))
            .filter(|s| !in_plan.contains(&s.id))
            .filter(|s| self.project_of(&s.root()).as_deref() == Some(project))
            .map(|s| s.id.clone())
            .collect();
        for id in &idle {
            self.close(id);
        }
        idle.len()
    }

    /// Starts the exited session `id` again in its place, from a record of it: its tile closes and its
    /// agent picks its conversation up again. A resumed scratch session keeps its folder, and finds its
    /// sandbox by its record.
    pub(super) fn resume_exited(&mut self, id: &str) {
        let Some(index) = self.sessions.iter().position(|s| s.id == id) else { return };
        let root = crate::build_cache::root(&self.sessions[index].root());
        if self.lifecycle_jobs.iter().any(|job| !job.is_finished() && job.touches(None, Some(&root))) {
            self.toast("Worktree cleanup is still running; resume once it finishes.");
            return;
        }
        let s = &self.sessions[index];
        let record = SessionRecord {
            chief: s.chief,
            pane: Some(s.id.clone()),
            claude_session_id: s.claude_id.clone().unwrap_or_default(),
            agent: s.agent.clone(),
            cwd: s.cwd.clone(),
            title: self.session_title(s),
            last_prompt: None,
            updated: 0,
            sandbox: s.sandbox.clone(),
            named: s.naming == Naming::Named,
        };
        if self.sessions[index].scratch.take().is_some() && record.sandbox.is_some() {
            self.store.upsert(record.clone());
        }
        self.close_for_resume(id);
        self.resume.push(record);
    }
}

/// What the slot queue holds for the process tree under `pid`.
pub(super) fn queue_note(pid: u32, running: &[Ticket], waiting: &[Ticket]) -> Option<String> {
    if let Some(ticket) = running.iter().find(|t| t.ancestors.contains(&pid)) {
        return Some(format!("{} slot", ticket.class.name()));
    }
    let ticket = waiting.iter().find(|t| t.ancestors.contains(&pid))?;
    let place = waiting.iter().filter(|t| t.class == ticket.class).position(|t| t.id == ticket.id)? + 1;
    Some(format!("queued for a {} slot ({place} in line)", ticket.class.name()))
}

/// The desktop's [`TerminalSurface`]: each session's terminal drawn by the desktop's own terminal view,
/// one per egui id.
pub(super) struct Terminals<'a> {
    pub(super) sessions: &'a [Session],
    pub(super) editors: &'a HashMap<PathBuf, Terminal>,
    pub(super) views: &'a mut HashMap<egui::Id, TerminalView>,
}

impl TerminalSurface for Terminals<'_> {
    fn show(
        &mut self,
        ui: &mut egui::Ui,
        index: usize,
        session: &str,
        rect: Rect,
        id: egui::Id,
        metrics: &Metrics,
        palette: &Palette,
    ) -> egui::Response {
        let terminal =
            self.sessions.get(index).filter(|s| s.id == session).and_then(|s| s.terminal.as_ref()).or_else(|| {
                self.editors
                    .iter()
                    .find(|(root, _)| atomics_core::projection::editor_terminal(&root.to_string_lossy()) == session)
                    .map(|(_, t)| t)
            });
        match terminal {
            Some(terminal) => self.views.entry(id).or_default().show(terminal, ui, rect, id, metrics, palette),
            // Not reached: views ask only for sessions with a terminal, from the same model.
            None => ui.interact(rect, id, Sense::click()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::backend::{resumes_as_codex, worktree_kept};
    use super::super::tests::{repository, session, with_app};
    use super::*;

    /// A task's result on a completed-task tile, from what the in-process backend reads for it.
    fn task_result(
        ui: &mut egui::Ui,
        task: &Task,
        status: Option<AgentStatus>,
        expanded: bool,
        read_only: bool,
        config: &Config,
        p: &Palette,
    ) -> Option<&'static str> {
        let (available, codex) = (worktree_kept(task), resumes_as_codex(config, task));
        let now = atomics_core::store::now();
        atomics_ui::board::task_result(ui, task, status, expanded, read_only, available, codex, now, p)
    }

    #[test]
    fn queue_notes_name_the_slot_or_the_place_in_line() {
        let ticket =
            |id, class, ancestors: &[u32]| Ticket { id, class, label: String::new(), ancestors: ancestors.to_vec() };
        let running = [ticket(1, SlotClass::Test, &[10, 1])];
        let waiting =
            [ticket(2, SlotClass::Qa, &[30]), ticket(3, SlotClass::Test, &[40]), ticket(4, SlotClass::Test, &[50])];
        assert_eq!(queue_note(10, &running, &waiting).as_deref(), Some("test slot"));
        assert_eq!(queue_note(50, &running, &waiting).as_deref(), Some("queued for a test slot (2 in line)"));
        assert_eq!(queue_note(30, &running, &waiting).as_deref(), Some("queued for a qa slot (1 in line)"));
        assert_eq!(queue_note(99, &running, &waiting), None);
    }

    #[test]
    fn all_view_only_shows_completed_tiles_from_open_plans() {
        with_app("app::fleet::tests::all_view_only_shows_completed_tiles_from_open_plans", |app, _| {
            let dir = crate::test_support::tempdir().unwrap();
            app.runtime.sessions.clear();
            app.runtime.plans = [PlanStatus::Open, PlanStatus::Completed, PlanStatus::Archived]
                .into_iter()
                .enumerate()
                .map(|(index, status)| Plan {
                    id: format!("plan-{index}"),
                    status,
                    project: Some(dir.path().into()),
                    tasks: vec![Task {
                        finished_at: Some(100),
                        worktree: Some(dir.path().join("removed")),
                        ..Task::default()
                    }],
                    ..Plan::default()
                })
                .collect();
            app.runtime.plan_view = false;
            app.runtime.view = None;
            assert_eq!(app.runtime.completed_tasks(), [(0, 0)]);
            app.runtime.view = Some(dir.path().into());
            assert_eq!(app.runtime.completed_tasks(), [(0, 0), (1, 0)]);
            app.runtime.plan_view = true;
            app.runtime.selected_plan = 1;
            assert_eq!(app.runtime.completed_tasks(), [(1, 0)], "completed plan history stays visible in its own view");
            app.runtime.selected_plan = 2;
            assert!(app.runtime.completed_tasks().is_empty());
        });
    }

    #[test]
    fn completed_task_resume_detects_codex_by_program_instead_of_agent_name() {
        with_app(
            "app::fleet::tests::completed_task_resume_detects_codex_by_program_instead_of_agent_name",
            |app, ctx| {
                let dir = crate::test_support::tempdir().unwrap();
                for (name, program, expected) in
                    [("custom-codex", "/tools/codex", true), ("codex", "/tools/other", false)]
                {
                    app.runtime.config.agents = vec![atomics_core::config::AgentConfig {
                        name: name.into(),
                        command: vec![program.into()],
                        kind: AgentKind::Generic,
                        busy_text: None,
                        isolation: None,
                    }];
                    let task = Task {
                        agent: name.into(),
                        finished_at: Some(100),
                        worktree: Some(dir.path().into()),
                        ..Task::default()
                    };
                    let render = |events| {
                        let mut action = None;
                        let output = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| {
                            egui::CentralPanel::default().show(ui, |ui| {
                                action =
                                    task_result(ui, &task, None, false, false, &app.runtime.config, &app.palette());
                            });
                        });
                        let pos = output
                            .shapes
                            .iter()
                            .find_map(|shape| match &shape.shape {
                                egui::epaint::Shape::Text(t) if t.galley.text() == "Resume" => {
                                    Some(t.pos + t.galley.rect.center().to_vec2())
                                }
                                _ => None,
                            })
                            .unwrap();
                        output.drop_without_applying_deltas();
                        (pos, action)
                    };
                    let (pos, _) = render(vec![]);
                    for pressed in [true, false] {
                        let (_, action) = render(vec![
                            egui::Event::PointerMoved(pos),
                            egui::Event::PointerButton {
                                pos,
                                button: egui::PointerButton::Primary,
                                pressed,
                                modifiers: egui::Modifiers::NONE,
                            },
                        ]);
                        if !pressed {
                            assert_eq!(action, expected.then_some("resume"), "{name}: {program}");
                        }
                    }
                }
            },
        );
    }

    #[test]
    fn removed_worktrees_disable_completed_result_actions() {
        with_app("app::fleet::tests::removed_worktrees_disable_completed_result_actions", |app, ctx| {
            let (_dir, repo) = repository();
            let worktree = repo.with_file_name("task");
            git::add_worktree(&repo, &worktree, "task", "main").unwrap();
            app.runtime.plans = vec![Plan {
                id: "cleaned".into(),
                status: PlanStatus::Completed,
                project: Some(repo),
                tasks: vec![Task {
                    branch: Some("task".into()),
                    finished_at: Some(100),
                    worktree: Some(worktree.clone()),
                    resume_id: Some("resume-id".into()),
                    ..Task::default()
                }],
                ..Plan::default()
            }];
            app.runtime.selected_plan = 0;
            let task = app.runtime.plans[0].tasks[0].clone();
            let mut time = 0.0;
            let mut render = |events| {
                time += 0.05;
                let mut action = None;
                let output = ctx.run_ui(egui::RawInput { time: Some(time), events, ..Default::default() }, |ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        action = task_result(ui, &task, None, false, false, &app.runtime.config, &app.palette());
                    });
                });
                let text: Vec<_> = output
                    .shapes
                    .iter()
                    .filter_map(|s| match &s.shape {
                        egui::epaint::Shape::Text(t) => {
                            Some((t.galley.text().to_string(), t.pos + t.galley.rect.center().to_vec2()))
                        }
                        _ => None,
                    })
                    .collect();
                output.drop_without_applying_deltas();
                (text, action)
            };
            for available in [true, false] {
                // The buttons' words, and the actions they report.
                for (label, expected) in [("Diff", "diff"), ("Resume", "resume")] {
                    let (text, _) = render(vec![]);
                    let pos = text.iter().find(|(t, _)| t == label).unwrap().1;
                    for pressed in [true, false] {
                        let (_, action) = render(vec![
                            egui::Event::PointerMoved(pos),
                            egui::Event::PointerButton {
                                pos,
                                button: egui::PointerButton::Primary,
                                pressed,
                                modifiers: egui::Modifiers::NONE,
                            },
                        ]);
                        if !pressed {
                            assert_eq!(action, available.then_some(expected));
                        }
                    }
                }
                if available {
                    let rows = atomics_core::cleanup::preview(&app.runtime.plans[0], "main").unwrap();
                    atomics_core::cleanup::remove(&app.runtime.plans[0], "main", &rows[0]).unwrap();
                    assert!(!worktree.exists());
                }
            }
            app.runtime.active = None;
            app.runtime.diff_root = None;
            let sessions = app.runtime.sessions.len();
            for action in ["diff", "resume"] {
                app.runtime.task_action(0, 0, action);
                assert!(app.runtime.diff_target().is_none());
                assert_eq!(app.runtime.sessions.len(), sessions);
                assert_eq!(app.runtime.plans[0].status, PlanStatus::Completed);
            }
        });
    }

    #[test]
    fn completed_tiles_and_board_show_results_actions_and_idle_report_age() {
        with_app(
            "app::fleet::tests::completed_tiles_and_board_show_results_actions_and_idle_report_age",
            |app, ctx| {
                let dir = crate::test_support::tempdir().unwrap();
                let root = dir.path();
                app.runtime.sessions =
                    (0..3).map(|i| session(&format!("live-{i}"), AgentKind::Claude, AgentStatus::Idle, root)).collect();
                let index = app.runtime.selected_plan;
                let completed = Task {
                    id: "done".into(),
                    title: "Finished task".into(),
                    status: TaskStatus::Done,
                    dispatched_at: Some(100),
                    finished_at: Some(890),
                    worktree: Some(root.into()),
                    resume_id: Some("resume-id".into()),
                    report: Some(Report {
                        status: "done".into(),
                        summary: "Report first line\nReport second line\nReport third line\nHidden fourth line".into(),
                        at: 890,
                        checks: Vec::new(),
                    }),
                    additions: 412,
                    deletions: 88,
                    ..Task::default()
                };
                app.runtime.plans[index] = Plan {
                    id: "results".into(),
                    goal: "Results".into(),
                    tasks: vec![
                        completed,
                        Task {
                            id: "waiting".into(),
                            status: TaskStatus::Review,
                            session: Some("live-0".into()),
                            report: Some(Report { at: atomics_core::store::now() - 1500, ..Report::default() }),
                            ..Task::default()
                        },
                    ],
                    ..Plan::default()
                };
                let mut time = 0.0;
                let mut render = |app: &mut Desktop<'_>, board: bool, events| {
                    time += 0.05;
                    let output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1600.0, 1000.0))),
                            time: Some(time),
                            events,
                            ..Default::default()
                        },
                        |ui| {
                            egui::CentralPanel::default().show(ui, |ui| {
                                if board {
                                    app.plan_panel(ui, ui.max_rect());
                                } else {
                                    let layout =
                                        layout::compute(1600.0, 1000.0, 3, 1, false, 0.0, &app.runtime.config.layout);
                                    assert_eq!(layout.completed, 1);
                                    app.tiles(ui, ui.max_rect(), &layout, &[0, 1, 2]);
                                }
                            });
                            app.dialogs(ctx);
                        },
                    );
                    app.runtime.drain_actions();
                    for shape in &output.shapes {
                        if let egui::epaint::Shape::Text(text) = &shape.shape
                            && text.galley.text().starts_with("Agent completed in")
                        {
                            assert!(!text.galley.job.justify, "completion text must keep normal word spacing");
                        }
                    }
                    let text: Vec<_> = output
                        .shapes
                        .iter()
                        .filter_map(|s| match &s.shape {
                            egui::epaint::Shape::Text(t) => {
                                Some((t.galley.text().to_string(), t.pos + t.galley.rect.center().to_vec2()))
                            }
                            _ => None,
                        })
                        .collect();
                    output.drop_without_applying_deltas();
                    text
                };
                for board in [false, true] {
                    let text = render(app, board, vec![]);
                    for expected in [
                        "Finished task",
                        "Agent completed in 13m 10s",
                        "+412 −88",
                        "Diff",
                        "Resume",
                        "reported 25 min ago",
                    ] {
                        assert!(text.iter().any(|(t, _)| t == expected), "missing {expected}: {text:?}");
                    }
                    assert!(
                        text.iter().any(|(t, _)| t.contains("Report first line") && t.contains("Report third line"))
                    );
                    assert!(!text.iter().any(|(t, _)| t.contains("Hidden fourth line")));
                }
                app.runtime.sessions[0].status = AgentStatus::Working;
                for board in [false, true] {
                    assert!(!render(app, board, vec![]).iter().any(|(t, _)| t == "reported 25 min ago"));
                }
                let text = render(app, false, vec![]);
                let pos = text.iter().find(|(t, _)| t == "Diff").unwrap().1;
                for pressed in [true, false] {
                    render(
                        app,
                        false,
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
                assert_eq!(app.runtime.diff_target().as_deref(), Some(root));
                app.runtime.plans[index].status = plan::PlanStatus::Completed;
                let text = render(app, true, vec![]);
                let pos = text.iter().find(|(t, _)| t == "Archive").unwrap().1;
                for pressed in [true, false] {
                    render(
                        app,
                        true,
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
                assert_eq!(app.runtime.plans[index].status, plan::PlanStatus::Archived);
            },
        );
    }
}
