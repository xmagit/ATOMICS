use super::*;
use imacs_core::cleanup::{self, Entry};

pub(super) struct PlanCleanup {
    plan: Plan,
    rows: Vec<(Entry, bool)>,
    busy: bool,
    log: Vec<String>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
}

enum Event {
    Preview(Result<Vec<Entry>, String>),
    Removed(Entry, Result<(), String>),
    Finished,
}

impl App {
    pub(super) fn open_plan_cleanup(&mut self, index: usize) {
        let (tx, rx) = channel();
        let plan = self.plans[index].clone();
        let mut dialog = PlanCleanup { plan, rows: Vec::new(), busy: false, log: Vec::new(), tx, rx };
        self.load_plan_cleanup(&mut dialog);
        self.plan_cleanup = Some(dialog);
    }

    fn load_plan_cleanup(&self, dialog: &mut PlanCleanup) {
        dialog.busy = true;
        dialog.rows.clear();
        let (tx, plan, base, ctx) =
            (dialog.tx.clone(), dialog.plan.clone(), self.config.base_ref.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Event::Preview(cleanup::preview(&plan, &base).map_err(|err| format!("{err:#}"))));
            ctx.request_repaint();
        });
    }

    pub(super) fn plan_cleanup_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.plan_cleanup.take() else { return };
        while let Ok(event) = dialog.rx.try_recv() {
            match event {
                Event::Preview(result) => {
                    dialog.busy = false;
                    match result {
                        Ok(rows) => {
                            dialog.rows = rows
                                .into_iter()
                                .map(|row| {
                                    let selected = row.keep.is_none();
                                    (row, selected)
                                })
                                .collect()
                        }
                        Err(err) => dialog.log.push(err),
                    }
                }
                Event::Removed(entry, result) => match result {
                    Ok(()) => {
                        dialog.log.push(format!("Removed {} and its worktree, if present", entry.branch));
                        dialog.rows.retain(|(row, _)| row != &entry);
                    }
                    Err(err) => dialog.log.push(format!("Kept {}: {err}", entry.branch)),
                },
                Event::Finished => {
                    dialog.busy = false;
                    self.worktrees_at = None;
                    self.load_plan_cleanup(&mut dialog);
                }
            }
        }
        let completed = self.plans.iter().any(|plan| plan.id == dialog.plan.id && plan.status == PlanStatus::Completed);
        let roots: HashSet<_> = self.sessions.iter().map(Session::root).collect();
        let mut open = true;
        let mut remove = false;
        let mut reload = false;
        egui::Window::new(format!("Clean up plan · {}", dialog.plan.goal))
            .id(egui::Id::unique("plan-cleanup"))
            .open(&mut open)
            .collapsible(false)
            .default_width(850.0)
            .show(ctx, |ui| {
                ui.label("Selected branches and their listed worktrees will be removed. Kept entries stay on disk.");
                if dialog.busy {
                    ui.spinner();
                } else if dialog.rows.is_empty() {
                    ui.label("No plan branches or worktrees to remove.");
                }
                egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                    for (entry, selected) in &mut dialog.rows {
                        let agent = entry.worktree.as_ref().is_some_and(|path| roots.contains(path));
                        let reason = entry
                            .keep
                            .as_deref()
                            .or(agent.then_some("an agent session is using this worktree"))
                            .or((!completed).then_some("plan is no longer completed"));
                        if reason.is_some() {
                            *selected = false;
                        }
                        ui.horizontal(|ui| {
                            ui.add_enabled(
                                !dialog.busy && reason.is_none(),
                                egui::Checkbox::new(selected, &entry.branch),
                            );
                            let label = reason.map(|why| format!("Keep: {why}")).unwrap_or_else(|| "Remove".into());
                            ui.label(RichText::new(label).color(if reason.is_some() {
                                self.palette.warning
                            } else {
                                self.palette.success
                            }));
                        });
                        let path =
                            entry.worktree.as_ref().map(|p| paths::display(p)).unwrap_or_else(|| "branch only".into());
                        ui.label(
                            RichText::new(format!("{} · {path}", paths::display(&entry.repo)))
                                .small()
                                .color(self.palette.muted),
                        );
                        ui.separator();
                    }
                });
                let selected = dialog.rows.iter().filter(|(_, selected)| *selected).count();
                ui.horizontal(|ui| {
                    remove = ui
                        .add_enabled(
                            !dialog.busy && selected > 0,
                            egui::Button::new(format!("Remove {selected} selected branches and worktrees")),
                        )
                        .clicked();
                    reload = ui.add_enabled(!dialog.busy, egui::Button::new("Reload preview")).clicked();
                });
                for line in &dialog.log {
                    ui.label(line);
                }
            });
        if reload {
            if let Some(plan) = self.plans.iter().find(|plan| plan.id == dialog.plan.id) {
                dialog.plan = plan.clone();
            }
            self.load_plan_cleanup(&mut dialog);
        }
        if remove
            && let Some(plan) = self
                .plans
                .iter()
                .find(|plan| plan.id == dialog.plan.id && plan.status == PlanStatus::Completed)
                .cloned()
        {
            dialog.busy = true;
            let targets: Vec<_> =
                dialog.rows.iter().filter(|(_, selected)| *selected).map(|(row, _)| row.clone()).collect();
            let (tx, plan, base, ctx) = (dialog.tx.clone(), plan, self.config.base_ref.clone(), self.ctx.clone());
            std::thread::spawn(move || {
                for entry in targets {
                    let result = cleanup::remove(&plan, &base, &entry).map_err(|err| format!("{err:#}"));
                    let _ = tx.send(Event::Removed(entry, result));
                    ctx.request_repaint();
                }
                let _ = tx.send(Event::Finished);
                ctx.request_repaint();
            });
        }
        if open {
            self.plan_cleanup = Some(dialog);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_plan_board_previews_kept_work_and_removes_only_confirmed_entries() {
        super::super::tests::with_app(
            "app::plan_cleanup::tests::completed_plan_board_previews_kept_work_and_removes_only_confirmed_entries",
            |app, ctx| {
                let dir = tempfile::tempdir().unwrap();
                let repo = dir.path().join("app");
                std::fs::create_dir(&repo).unwrap();
                for args in [
                    vec!["init", "-q", "-b", "main"],
                    vec!["config", "user.email", "test@example.com"],
                    vec!["config", "user.name", "Test"],
                    vec!["commit", "--allow-empty", "-qm", "base"],
                ] {
                    git::git(&repo, &args).unwrap();
                }
                let mut plan = Plan {
                    id: "ship".into(),
                    goal: "Ship cleanup".into(),
                    project: Some(repo.clone()),
                    status: PlanStatus::Completed,
                    ..Plan::default()
                };
                for branch in ["clean", "dirty", "unmerged", "busy", "another-plan"] {
                    let path = dir.path().join(branch);
                    git::add_worktree(&repo, &path, branch, "main").unwrap();
                    if branch != "another-plan" {
                        plan.tasks.push(Task {
                            branch: Some(branch.into()),
                            status: TaskStatus::Done,
                            ..Task::default()
                        });
                    }
                }
                std::fs::write(dir.path().join("dirty/new-file"), "keep dirty work").unwrap();
                std::fs::write(dir.path().join("unmerged/new-file"), "keep commit").unwrap();
                git::commit_all(&dir.path().join("unmerged"), "unmerged").unwrap();
                git::git(&repo, &["branch", &plan.integration_branch()]).unwrap();
                app.sessions.push(Session::starting(
                    "busy".into(),
                    &app.config.agents[0],
                    dir.path().join("busy"),
                    false,
                    &app.ctx,
                ));
                app.plans = vec![plan];
                app.selected_plan = 0;
                app.config.base_ref = "main".into();
                let mut time = 0.0;
                let mut frame = |app: &mut App, events| {
                    time += 0.05;
                    let output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1700.0, 1000.0))),
                            time: Some(time),
                            events,
                            ..Default::default()
                        },
                        |ui| {
                            egui::Panel::top("test-tabs").show(ui, |ui| {
                                ui.horizontal(|ui| app.view_tabs(ui));
                            });
                            egui::CentralPanel::default().show(ui, |ui| {
                                if app.panel == PanelMode::Plan {
                                    app.plan_panel(ui, ui.max_rect());
                                }
                            });
                            app.plan_cleanup_dialog(ctx);
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
                let mut click = |app: &mut App, label: &str| {
                    let text = frame(app, vec![]);
                    let pos =
                        text.iter().find(|(s, _)| s == label).unwrap_or_else(|| panic!("missing {label}: {text:?}")).1;
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
                };
                click(app, "✓ Ship cleanup · 4/4");
                app.toggle_plan();
                assert!(app.plan_view && app.diff_open && app.panel == PanelMode::Plan);
                click(app, "Clean up plan…");
                let deadline = Instant::now() + Duration::from_secs(10);
                while app.plan_cleanup.as_ref().unwrap().busy {
                    assert!(Instant::now() < deadline);
                    frame(app, vec![]);
                    std::thread::sleep(Duration::from_millis(10));
                }
                let text = frame(app, vec![]);
                for reason in ["uncommitted changes", "unmerged commits", "an agent session is using"] {
                    assert!(text.iter().any(|(s, _)| s.contains(reason)), "missing {reason}: {text:?}");
                }
                let dialog = app.plan_cleanup.as_ref().unwrap();
                assert_eq!(dialog.rows.len(), 5);
                assert_eq!(dialog.rows.iter().filter(|(_, selected)| *selected).count(), 2);
                assert!(dir.path().join("clean").exists(), "opening the preview never deletes work");
                for label in ["imacs/plan-ship", "Remove 1 selected branches and worktrees"] {
                    let text = frame(app, vec![]);
                    let pos = text.iter().find(|(s, _)| s == label).unwrap().1;
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
                }
                let deadline = Instant::now() + Duration::from_secs(10);
                while app.plan_cleanup.as_ref().unwrap().busy {
                    assert!(Instant::now() < deadline);
                    frame(app, vec![]);
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(!dir.path().join("clean").exists());
                for branch in ["dirty", "unmerged", "busy", "another-plan"] {
                    assert!(dir.path().join(branch).exists());
                    assert!(git::git(&repo, &["show-ref", "--verify", &format!("refs/heads/{branch}")]).is_ok());
                }
                assert!(git::git(&repo, &["show-ref", "--verify", "refs/heads/imacs/plan-ship"]).is_ok());
                assert_eq!(app.plans[0].status, PlanStatus::Completed);
                app.plans[0].status = PlanStatus::Open;
                let text = frame(app, vec![]);
                assert!(text.iter().any(|(s, _)| s.contains("plan is no longer completed")));
                assert!(app.plan_cleanup.as_ref().unwrap().rows.iter().all(|(_, selected)| !selected));
            },
        );
    }
}
