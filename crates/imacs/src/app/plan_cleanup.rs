use super::*;
use imacs_core::cleanup::{self, Entry};

pub(super) struct PlanCleanup {
    plan: Plan,
    rows: Vec<Entry>,
    pub(super) busy: bool,
    log: Vec<String>,
    tx: crate::host::Sender<Event>,
    rx: Receiver<Event>,
}

#[cfg(test)]
impl PlanCleanup {
    pub(super) fn ready(&self) -> bool {
        !self.busy
    }
}

enum Event {
    Preview(Result<Vec<Entry>, String>),
    Removed(Entry, Result<(), String>),
    Note(String),
    Finished,
}

impl State {
    pub(super) fn open_plan_cleanup(&mut self, index: usize) {
        let (tx, rx) = self.wake.channel();
        let plan = self.plans[index].clone();
        let mut dialog = PlanCleanup {
            plan,
            rows: Vec::new(),
            busy: false,
            log: crate::lifecycle::leftovers(&paths::imacs_home()),
            tx,
            rx,
        };
        self.load_plan_cleanup(&mut dialog);
        self.plan_cleanup = Some(dialog);
        self.client_events.push(ClientEvent::PlanCleanup);
    }

    fn load_plan_cleanup(&self, dialog: &mut PlanCleanup) {
        dialog.busy = true;
        dialog.rows.clear();
        let (tx, plan, base) = (dialog.tx.clone(), dialog.plan.clone(), self.plan_base_ref(&dialog.plan));
        std::thread::spawn(move || {
            let result =
                crate::sandbox::load_owned_pins(&paths::imacs_home()).and_then(|()| cleanup::preview(&plan, &base));
            let _ = tx.send(Event::Preview(result.map_err(|err| format!("{err:#}"))));
        });
    }

    pub(super) fn poll_plan_cleanup(&mut self) -> bool {
        let Some(mut dialog) = self.plan_cleanup.take() else { return false };
        let mut changed = false;
        while let Ok(event) = dialog.rx.try_recv() {
            changed = true;
            match event {
                Event::Preview(result) => {
                    dialog.busy = false;
                    match result {
                        Ok(rows) => {
                            self.client_events.push(ClientEvent::PlanCleanupPreview(
                                rows.iter()
                                    .filter(|row| row.keep.is_none())
                                    .map(|row| (row.repo.clone(), row.branch.clone()))
                                    .collect(),
                            ));
                            dialog.rows = rows;
                        }
                        Err(err) => dialog.log.push(err),
                    }
                }
                Event::Removed(entry, result) => match result {
                    Ok(()) => {
                        dialog.log.push(format!("Removed {} and its worktree, if present", entry.branch));
                        dialog.rows.retain(|row| row != &entry);
                    }
                    Err(err) => dialog.log.push(format!("Kept {}: {err}", entry.branch)),
                },
                Event::Note(note) => dialog.log.push(note),
                Event::Finished => {
                    dialog.busy = false;
                    self.worktrees_at = None;
                    self.load_plan_cleanup(&mut dialog);
                }
            }
        }
        self.plan_cleanup = Some(dialog);
        changed
    }
}

impl Desktop<'_> {
    pub(super) fn plan_cleanup_dialog(&mut self, ctx: &egui::Context) {
        self.apply_client_events(ctx);
        if !self.client.plan_cleanup_open {
            return;
        }
        let Some(mut dialog) = self.runtime.plan_cleanup.take() else { return };
        let completed =
            self.runtime.plans.iter().any(|plan| plan.id == dialog.plan.id && plan.status == PlanStatus::Completed);
        // Sessions resolve their worktree in the background, so compare real paths (/var is /private/var).
        let real = |path: std::path::PathBuf| std::fs::canonicalize(&path).unwrap_or(path);
        let roots: HashSet<_> = self.runtime.sessions.iter().map(|s| real(s.root())).collect();
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
                    for entry in &dialog.rows {
                        let key = (entry.repo.clone(), entry.branch.clone());
                        let mut selected = self.client.plan_cleanup_selected.contains(&key);
                        let agent = entry.worktree.clone().is_some_and(|path| roots.contains(&real(path)));
                        let reason = entry
                            .keep
                            .as_deref()
                            .or(agent.then_some("an agent session is using this worktree"))
                            .or((!completed).then_some("plan is no longer completed"));
                        if reason.is_some() {
                            selected = false;
                            self.client.plan_cleanup_selected.remove(&key);
                        }
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(
                                    !dialog.busy && reason.is_none(),
                                    egui::Checkbox::new(&mut selected, &entry.branch),
                                )
                                .changed()
                            {
                                if selected {
                                    self.client.plan_cleanup_selected.insert(key.clone());
                                } else {
                                    self.client.plan_cleanup_selected.remove(&key);
                                }
                            }
                            let label = reason.map(|why| format!("Keep: {why}")).unwrap_or_else(|| "Remove".into());
                            ui.label(RichText::new(label).color(if reason.is_some() {
                                self.runtime.palette.warning
                            } else {
                                self.runtime.palette.success
                            }));
                        });
                        let path =
                            entry.worktree.as_ref().map(|p| paths::display(p)).unwrap_or_else(|| "branch only".into());
                        ui.label(
                            RichText::new(format!("{} · {path}", paths::display(&entry.repo)))
                                .small()
                                .color(self.runtime.palette.muted),
                        );
                        ui.separator();
                    }
                });
                let selected = dialog
                    .rows
                    .iter()
                    .filter(|row| self.client.plan_cleanup_selected.contains(&(row.repo.clone(), row.branch.clone())))
                    .count();
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
            if let Some(plan) = self.runtime.plans.iter().find(|plan| plan.id == dialog.plan.id) {
                dialog.plan = plan.clone();
            }
            self.post(|state| {
                if let Some(mut dialog) = state.plan_cleanup.take() {
                    state.load_plan_cleanup(&mut dialog);
                    state.plan_cleanup = Some(dialog);
                }
            });
        }
        if remove
            && let Some(plan) = self
                .plans
                .iter()
                .find(|plan| plan.id == dialog.plan.id && plan.status == PlanStatus::Completed)
                .cloned()
        {
            dialog.busy = true;
            let targets: Vec<_> = dialog
                .rows
                .iter()
                .filter(|row| self.client.plan_cleanup_selected.contains(&(row.repo.clone(), row.branch.clone())))
                .cloned()
                .collect();
            let (tx, plan, base) = (dialog.tx.clone(), plan.clone(), self.runtime.plan_base_ref(&plan));
            let sbx = resolve_program(&self.runtime.sbx);
            self.post(move |state| {
                state.lifecycle_jobs.push(std::thread::spawn(move || {
                    for entry in targets {
                        // Which sandboxes work there is asked while the worktree is still on disk.
                        let sandboxes = match &entry.worktree {
                            Some(path) if crate::sandbox::installed(&sbx) => {
                                match crate::sandbox::in_worktree(&paths::imacs_home(), &sbx, path) {
                                    Ok(names) => names,
                                    Err(err) => {
                                        let _ = tx.send(Event::Note(format!(
                                            "Kept {}: listing sandboxes failed: {err:#}",
                                            entry.branch
                                        )));
                                        continue;
                                    }
                                }
                            }
                            _ => Vec::new(),
                        };
                        if let Some(error) = sandboxes
                            .iter()
                            .find_map(|name| crate::sandbox::stop(&paths::imacs_home(), &sbx, name).err())
                        {
                            let _ = tx.send(Event::Note(format!(
                                "Kept {}: stopping sandbox failed: {error:#}",
                                entry.branch
                            )));
                            continue;
                        }
                        let write = crate::ref_guard::host_write(&entry.repo, [&entry.branch]);
                        let result = cleanup::remove(&plan, &base, &entry).map_err(|err| format!("{err:#}"));
                        drop(write);
                        let removed = result.is_ok();
                        let _ = tx.send(Event::Removed(entry, result));
                        for name in sandboxes.into_iter().filter(|_| removed) {
                            let note = match crate::sandbox::remove(&paths::imacs_home(), &sbx, &name) {
                                Ok(()) => format!("Removed sandbox {name}"),
                                Err(err) => format!("Kept sandbox {name}: {err:#}"),
                            };
                            let _ = tx.send(Event::Note(note));
                        }
                    }
                    let _ = tx.send(Event::Finished);
                }));
            });
        }
        self.client.plan_cleanup_open = open;
        self.runtime.plan_cleanup = Some(dialog);
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
                let dir = crate::test_support::tempdir().unwrap();
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
                app.runtime.sessions.push(Session::starting(
                    "busy".into(),
                    &app.runtime.config.agents[0],
                    dir.path().join("busy"),
                    false,
                    &app.runtime.wake,
                ));
                app.runtime.plans = vec![plan];
                app.runtime.selected_plan = 0;
                app.runtime.config.base_ref = "main".into();
                let mut time = 0.0;
                let mut frame = |app: &mut Desktop<'_>, events| {
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
                                if app.runtime.panel == PanelMode::Plan {
                                    app.plan_panel(ui, ui.max_rect());
                                }
                            });
                            app.plan_cleanup_dialog(ctx);
                        },
                    );
                    app.runtime.drain_actions();
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
                let mut click = |app: &mut Desktop<'_>, label: &str| {
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
                click(app, "✓ Ship cleanup");
                assert!(app.runtime.plan_view && app.runtime.diff_open && app.runtime.panel == PanelMode::Plan);
                click(app, "Clean up plan…");
                // The preview runs several git commands per worktree; while the whole suite runs, one
                // git call can take half a second. The bound only guards against a hang.
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(30));
                while app.runtime.plan_cleanup.as_ref().unwrap().busy {
                    assert!(Instant::now() < deadline);
                    app.runtime.runtime_tick(ctx);
                    std::thread::sleep(Duration::from_millis(10));
                }
                frame(app, vec![]); // egui sizes a newly opened window before painting it.
                let text = frame(app, vec![]);
                for reason in ["uncommitted changes", "unmerged commits", "an agent session is using"] {
                    assert!(text.iter().any(|(s, _)| s.contains(reason)), "missing {reason}: {text:?}");
                }
                let dialog = app.runtime.plan_cleanup.as_ref().unwrap();
                assert_eq!(dialog.rows.len(), 5);
                assert_eq!(app.client.plan_cleanup_selected.len(), 2);
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
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                while app.runtime.plan_cleanup.as_ref().unwrap().busy {
                    assert!(Instant::now() < deadline);
                    app.runtime.runtime_tick(ctx);
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(!dir.path().join("clean").exists());
                for branch in ["dirty", "unmerged", "busy", "another-plan"] {
                    assert!(dir.path().join(branch).exists());
                    assert!(git::git(&repo, &["show-ref", "--verify", &format!("refs/heads/{branch}")]).is_ok());
                }
                assert!(git::git(&repo, &["show-ref", "--verify", "refs/heads/imacs/plan-ship"]).is_ok());
                assert_eq!(app.runtime.plans[0].status, PlanStatus::Completed);
                app.runtime.plans[0].status = PlanStatus::Open;
                let text = frame(app, vec![]);
                assert!(text.iter().any(|(s, _)| s.contains("plan is no longer completed")));
                assert!(app.client.plan_cleanup_selected.is_empty());
            },
        );
    }
}
