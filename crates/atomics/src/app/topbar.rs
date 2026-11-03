use super::*;

pub(super) use atomics_ui::topbar::record_item;

impl Desktop<'_> {
    /// Phone and browser access through a rendezvous service, when `[remote.rendezvous]` is
    /// enabled: "Remote" in the success colour while online, with a dot in the warning colour while
    /// it waits or has stopped, or while the operation journal cannot be written; muted while it
    /// connects. The hover says why; a click opens Remote devices (pairing and revoking).
    fn rendezvous_indicator(&mut self, ui: &mut egui::Ui, in_menu: bool) {
        use crate::protocol::rendezvous::Phase;
        let Some(status) = self.runtime.rendezvous_status() else { return };
        let palette = self.palette();
        let journal = self.runtime.journal_problem();
        let (text, color) = match status.phase {
            _ if journal.is_some() => ("Remote ●".to_string(), palette.warning),
            Phase::Online => ("Remote".to_string(), palette.success),
            Phase::Waiting | Phase::Stopped => ("Remote ●".to_string(), palette.warning),
            Phase::Starting | Phase::Registering | Phase::Connecting => ("Remote …".to_string(), palette.muted),
        };
        let service = if status.host.is_empty() {
            status.service.clone()
        } else {
            format!("{} ({})", status.service, status.host)
        };
        let state = match status.phase {
            Phase::Online => match status.channels {
                0 => "online, no browser connected".to_string(),
                1 => "online, 1 browser connected".to_string(),
                n => format!("online, {n} browsers connected"),
            },
            Phase::Starting => "starting".into(),
            Phase::Registering => "registering this host".into(),
            Phase::Connecting => "connecting".into(),
            Phase::Waiting => "waiting to try again".into(),
            Phase::Stopped => "stopped".into(),
        };
        let mut hover = format!("Phone and browser access through {service}: {state}");
        if !status.registered && status.phase != Phase::Online {
            hover.push_str("\nThis host is not registered yet.");
        }
        for note in status.note.iter().chain(&journal) {
            hover.push('\n');
            hover.push_str(note);
        }
        if self.runtime.remote_devices().is_some() {
            hover.push_str("\nClick to pair a phone or browser, or to revoke one.");
        }
        let response = ui.selectable_label(self.client.remote_devices, RichText::new(text).color(color));
        record_item(ui, "Remote", &response);
        if response.on_hover_text(hover).clicked() && self.runtime.remote_devices().is_some() {
            self.client.remote_devices = !self.client.remote_devices;
            if in_menu {
                ui.close();
            }
        }
    }

    /// This host's items after the top bar's Plan button: Tickets, the GitHub counter, Remote, and in the
    /// Actions menu (`in_menu`) the ticket system's sign-in.
    pub(super) fn host_top_bar_items(&mut self, ui: &mut egui::Ui, in_menu: bool) {
        if self.runtime.ticket_config().is_some() {
            // A dot says the ticket system wants a sign-in.
            let signed_out = self.runtime.tickets.panel.signin;
            let text = RichText::new(if signed_out { "Tickets ●" } else { "Tickets" }).color(if signed_out {
                self.palette().warning
            } else {
                self.palette().foreground
            });
            let open = self.runtime.panel == PanelMode::Tickets && self.runtime.diff_open;
            let response = ui.selectable_label(open, text);
            record_item(ui, "Tickets", &response);
            if response.on_hover_text(shortcut_label(ui.ctx(), &self.runtime.config.keys.toggle_tickets)).clicked() {
                self.runtime.toggle_tickets();
                if in_menu {
                    ui.close();
                }
            }
        }
        self.github_counter(ui, in_menu);
        self.rendezvous_indicator(ui, in_menu);
        if in_menu && let Some(server) = self.runtime.sign_server() {
            let system = self.runtime.ticket_config().map(|c| c.system.clone()).unwrap_or_default();
            let signed_in = self.runtime.tickets.sign.state() == Some(&crate::ticket_signin::SignState::SignedIn)
                && !self.runtime.tickets.panel.signin;
            let label = if signed_in {
                format!("Tickets: sign out of {system}")
            } else {
                format!("Tickets: sign in to {system}")
            };
            if ui.button(label).clicked() {
                if signed_in {
                    self.runtime.start_sign_out();
                } else {
                    self.runtime.show_signin();
                }
                ui.close();
            }
            if signed_in
                && server.url.is_some()
                && ui.button(format!("Tickets: let sandboxed agents use {system}'s own tools")).clicked()
            {
                self.runtime.connect_ticket_server();
                ui.close();
            }
        }
    }

    /// The Settings menu, last in the top bar; ctl's `show_settings` opens or closes it.
    pub(super) fn settings_button(&mut self, ui: &mut egui::Ui) {
        // Trying themes and toggling where they are saved keep the menu open.
        let config =
            egui::containers::menu::MenuConfig::new().close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside);
        let (response, _) = if egui::containers::menu::is_in_menu(ui) {
            egui::containers::menu::SubMenuButton::new("Settings").config(config).ui(ui, |ui| self.settings_menu(ui))
        } else {
            egui::containers::menu::MenuButton::new("Settings").config(config).ui(ui, |ui| self.settings_menu(ui))
        };
        record_item(ui, "Settings", &response);
        if let Some(open) = self.client.show_settings.take() {
            let id = egui::Popup::default_response_id(&response);
            if open {
                egui::Popup::open_id(ui.ctx(), id);
            } else {
                egui::Popup::close_id(ui.ctx(), id);
            }
            ui.ctx().request_repaint();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{session, with_app};
    use super::*;

    fn render(
        app: &mut Desktop<'_>,
        ctx: &egui::Context,
        width: f32,
        events: Vec<egui::Event>,
    ) -> (Vec<(String, Rect)>, egui::FullOutput) {
        ctx.data_mut(|d| d.insert_temp(egui::Id::NULL.with("topbar-items"), Vec::<(String, Rect)>::new()));
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(width, 800.0))),
                events,
                ..Default::default()
            },
            |ui| {
                egui::Panel::top("top")
                    .exact_size(34.0)
                    .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(10, 4)))
                    .show(ui, |ui| app.top_bar(ui));
            },
        );
        app.runtime.drain_actions();
        let items = ctx.data(|d| d.get_temp::<Vec<(String, Rect)>>(egui::Id::NULL.with("topbar-items")).unwrap());
        (items, output)
    }

    fn click(app: &mut Desktop<'_>, ctx: &egui::Context, width: f32, label: &str) {
        let (items, output) = render(app, ctx, width, vec![]);
        let mut pos = items.iter().find(|(name, _)| name == label).map(|(_, r)| r.center());
        if pos.is_none() {
            pos = output.shapes.iter().find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text().starts_with(label) => {
                    Some(text.pos + text.galley.rect.center().to_vec2())
                }
                _ => None,
            });
        }
        let pos = pos.unwrap_or_else(|| panic!("missing {label}: {items:?}"));
        output.drop_without_applying_deltas();
        for pressed in [true, false] {
            render(
                app,
                ctx,
                width,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            )
            .1
            .drop_without_applying_deltas();
        }
    }

    fn fixture(app: &mut Desktop<'_>, count: usize) {
        app.runtime.repos = std::iter::once(PathBuf::from("/project/ATOMICS"))
            .chain((1..8).map(|i| PathBuf::from(format!("/project/repo-{i}"))))
            .collect();
        app.runtime.plans = (0..count)
            .map(|i| Plan {
                id: format!("plan-{i}"),
                goal: format!("Plan {i}: resource budgets and built-in approval rules"),
                status: if i % 2 == 0 { PlanStatus::Open } else { PlanStatus::Completed },
                tasks: vec![Task { id: "task".into(), title: "A task".into(), ..Task::default() }],
                ..Plan::default()
            })
            .collect();
        if count == 0 {
            app.runtime.plans.push(Plan::default());
        }
        app.runtime.selected_plan = count.saturating_sub(1);
        app.runtime.plan_view = true;
        app.runtime.sessions =
            vec![session("chief", AgentKind::Generic, AgentStatus::NeedsInput, Path::new("/project/ATOMICS"))];
        app.runtime.sessions[0].chief = true;
        app.runtime.toast = Some((
            "A very long notification that should never draw over the chief, tabs, or buttons\nMore details".repeat(4),
            Instant::now(),
        ));
        app.runtime.config_error = Some("Invalid config fixture".into());
        app.runtime.hook_error = Some("Offline hook fixture".into());
        app.runtime.heartbeat.last = Some((atomics_core::store::now(), vec![]));
        app.client.load = (Instant::now(), Some(123.4));
    }

    #[test]
    fn top_bar_items_never_overlap() {
        with_app("app::topbar::tests::top_bar_items_never_overlap", |app, ctx| {
            for count in [0, 3, 6] {
                fixture(app, count);
                let mut vm = session("vm", AgentKind::Generic, AgentStatus::Working, Path::new("/project/ATOMICS"));
                vm.sandbox = Some("atomics-vm".into());
                app.runtime.sessions.push(vm);
                app.runtime
                    .sandbox_usage
                    .usage
                    .insert("atomics-vm".into(), crate::sandbox_usage::Usage { bytes: 1_073_741_824, cpu: 120.0 });
                let mut action_widths = HashMap::new();
                for dock in [ChiefDock::Floating, ChiefDock::Docked] {
                    app.runtime.store.chief_dock = Some(dock);
                    app.runtime.chief_open = true;
                    for width in [480.0, 550.0, 700.0, 900.0, 1200.0, 1500.0, 2400.0, 5120.0] {
                        let (items, output) = render(app, ctx, width, vec![]);
                        let case = format!("width={width}, plans={count}, dock={dock:?}");
                        let bounds = Rect::from_min_size(pos2(10.0, 0.0), vec2(width - 20.0, 34.0)).expand(0.5);
                        for (i, (name, rect)) in items.iter().enumerate() {
                            assert!(bounds.contains_rect(*rect), "{case}: {name} outside bar: {rect:?}; {items:?}");
                            for (other, other_rect) in &items[i + 1..] {
                                assert!(
                                    !rect.shrink(0.1).intersects(*other_rect),
                                    "{case}: {name} {rect:?} overlaps {other} {other_rect:?}"
                                );
                            }
                        }
                        let collapsed = items.iter().any(|(name, _)| name == "Actions");
                        let required: &[&str] = if collapsed {
                            &["chief", "Actions", "Tabs"]
                        } else {
                            &["chief", "Settings", "Project", "Plan", "+ New", "diff"]
                        };
                        for &name in required {
                            assert_eq!(items.iter().filter(|(n, _)| n == name).count(), 1, "{case}: {name}");
                            if name != "chief" {
                                let width = items.iter().find(|(n, _)| n == name).unwrap().1.width();
                                let natural = action_widths.entry(name).or_insert(width);
                                assert!((width - *natural).abs() < 0.5, "{case}: {name} shrank");
                            }
                        }
                        // Check the rendered text too: distinct labels cannot paint
                        // across each other, even inside compound items such as CHIEF.
                        let mut text_rects = Vec::new();
                        for shape in &output.shapes {
                            let painted = shape.shape.visual_bounding_rect().intersect(shape.clip_rect);
                            let bar = Rect::from_min_size(Pos2::ZERO, vec2(width, 34.0)).expand(0.5);
                            assert!(
                                !painted.is_positive() || bar.contains_rect(painted),
                                "{case}: paint outside bar {painted:?}"
                            );
                            if let egui::Shape::Text(text) = &shape.shape {
                                let rect = text.galley.rect.translate(text.pos.to_vec2());
                                assert!(
                                    shape.clip_rect.expand(0.5).contains_rect(rect),
                                    "{case}: text outside slot: {} {rect:?}, clip {:?}",
                                    text.galley.text(),
                                    shape.clip_rect
                                );
                                for (other, other_rect) in &text_rects {
                                    assert!(
                                        !rect.shrink(0.1).intersects(*other_rect),
                                        "{case}: text {} overlaps {other}",
                                        text.galley.text()
                                    );
                                }
                                text_rects.push((text.galley.text(), rect));
                            }
                        }
                        if width == 5120.0 {
                            let chief = items.iter().find(|(n, _)| n == "chief").unwrap().1;
                            assert!((chief.center().x - width / 2.0).abs() < 1.0, "wide pill remains centered");
                        }
                        if width == 5120.0 {
                            assert!(!items.iter().any(|(n, _)| n == "Tabs"));
                            assert_eq!(items.iter().filter(|(n, _)| n.starts_with("Repos ·")).count(), 1);
                            assert!(!items.iter().any(|(n, _)| n.starts_with("repo-")));
                            assert!(items.iter().any(|(n, _)| n == "load"));
                            assert!(items.iter().any(|(n, _)| n == "sandbox-total"));
                            assert!(output.shapes.iter().any(|s| matches!(
                                &s.shape, egui::Shape::Text(t) if t.galley.text() == "VMs 1.0 GiB · 120% CPU"
                            )));
                        }
                        output.drop_without_applying_deltas();
                    }
                }
            }
        });
    }

    #[test]
    fn urgent_plans_stay_visible_or_are_surfaced_by_tabs_at_supported_widths() {
        with_app(
            "app::topbar::tests::urgent_plans_stay_visible_or_are_surfaced_by_tabs_at_supported_widths",
            |app, ctx| {
                fixture(app, 5);
                app.runtime.plans[0].status = PlanStatus::Open;
                app.runtime.plans[0].goal = "A long running plan name that must yield before its running count".into();
                app.runtime.plans[0].tasks = ["first", "second"]
                    .into_iter()
                    .map(|id| Task {
                        id: id.into(),
                        session: Some(id.into()),
                        status: TaskStatus::Running,
                        ..Task::default()
                    })
                    .collect();
                app.runtime.plans[1].status = PlanStatus::Open;
                app.runtime.plans[1].goal = "A needs-you plan with a long descriptive name".into();
                app.runtime.plans[1].tasks[0].status = TaskStatus::Blocked;
                app.runtime.plans[3].status = PlanStatus::Completed;
                app.runtime.plans[4].status = PlanStatus::Completed;
                for id in ["first", "second"] {
                    app.runtime.sessions.push(session(
                        id,
                        AgentKind::Generic,
                        AgentStatus::Working,
                        Path::new("/project/ATOMICS"),
                    ));
                }
                app.runtime.changed_plans.insert(app.runtime.plans[0].id.clone());
                app.runtime.changed_plans.insert(app.runtime.plans[1].id.clone());
                for width in [480.0, 900.0, 1200.0, 1400.0] {
                    let (items, output) = render(app, ctx, width, vec![]);
                    let labels: Vec<_> = output
                        .shapes
                        .iter()
                        .filter_map(|shape| if let egui::Shape::Text(text) = &shape.shape { Some(text) } else { None })
                        .collect();
                    let needs_visible = items
                        .iter()
                        .any(|(name, _)| name.starts_with("◆ ") && name.trim_end_matches(" *").ends_with("needs you"));
                    let surfaced = labels.iter().find(|text| {
                        text.galley.text().starts_with("Tabs · ") && text.galley.text().ends_with("needs you")
                    });
                    assert!(
                        needs_visible || surfaced.is_some(),
                        "width={width}: needs-you plan disappeared: {items:?}"
                    );
                    if let Some(text) = surfaced {
                        assert!(
                            text.galley
                                .job
                                .sections
                                .iter()
                                .any(|section| section.format.color == app.palette().warning)
                        );
                        assert!(!text.galley.elided, "width={width}: urgency suffix on Tabs was clipped");
                    }
                    for text in labels.iter().filter(|text| {
                        text.galley.text().starts_with("◆ ") && text.galley.text().contains("needs you")
                            || text.galley.text().starts_with("● ") && text.galley.text().contains("2 running")
                    }) {
                        assert!(
                            !text.galley.elided,
                            "width={width}: important state was elided: {}",
                            text.galley.text()
                        );
                        let bounds = text.galley.rect.translate(text.pos.to_vec2());
                        let clip = output
                            .shapes
                            .iter()
                            .find(|shape| matches!(&shape.shape,egui::Shape::Text(other) if std::ptr::eq(other,*text)))
                            .unwrap()
                            .clip_rect;
                        assert!(clip.expand(0.5).contains_rect(bounds), "width={width}: important label was clipped");
                    }
                    if width >= 900.0 {
                        assert!(needs_visible, "width={width}: needs-you should fit");
                        assert!(
                            items.iter().any(|(name, _)| name.starts_with("● ")
                                && name.trim_end_matches(" *").ends_with("2 running")),
                            "width={width}: running count disappeared: {items:?}"
                        );
                    }
                    output.drop_without_applying_deltas();
                }
            },
        );
    }

    #[test]
    fn project_views_are_reachable_through_repos_and_show_the_selected_project() {
        with_app(
            "app::topbar::tests::project_views_are_reachable_through_repos_and_show_the_selected_project",
            |app, ctx| {
                for width in [480.0, 1700.0] {
                    fixture(app, 3);
                    let root = PathBuf::from("/project/repo-1");
                    let worktree = PathBuf::from("/worktrees/repo-1-task");
                    app.runtime.worktree_project.insert(worktree.clone(), root.clone());
                    for (id, path, additions, deletions) in [
                        ("main", root.clone(), 10, 2),
                        ("task", worktree, 4, 1),
                        ("other", PathBuf::from("/project/repo-2"), 2, 3),
                        ("chief", PathBuf::from("/project/ATOMICS"), 100, 200),
                    ] {
                        if id != "chief" {
                            app.runtime.sessions.push(session(id, AgentKind::Generic, AgentStatus::Idle, &path));
                        }
                        app.runtime.diffs.entry(path).or_default().prepared = Some(Arc::new(diff_view::Prepared {
                            diff: atomics_core::diff::WorktreeDiff::default(),
                            highlights: vec![],
                            longest_line: 0,
                            additions,
                            deletions,
                            all_files: Arc::default(),
                        }));
                    }
                    app.runtime.active = Some("other".into());
                    app.runtime.view = None;
                    let (items, output) = render(app, ctx, width, vec![]);
                    let repos_visible = items.iter().any(|(name, _)| name == "Repos · 22");
                    assert!(!items.iter().any(|(name, _)| name.starts_with("repo-")));
                    output.drop_without_applying_deltas();
                    if !repos_visible {
                        click(app, ctx, width, "Tabs");
                    }
                    click(app, ctx, width, "Repos · 22");
                    // A submenu sizes before its first paint.
                    render(app, ctx, width, vec![]).1.drop_without_applying_deltas();
                    let (_, output) = render(app, ctx, width, vec![]);
                    for label in [
                        "repo-1 · 2 sessions · 17 changes",
                        "repo-2 · 1 sessions · 5 changes",
                        "repo-7 · 0 sessions · 0 changes",
                    ] {
                        assert!(
                            output.shapes.iter().any(
                                |shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text() == label)
                            ),
                            "width={width}: missing {label}"
                        );
                    }
                    output.drop_without_applying_deltas();
                    click(app, ctx, width, "repo-1 · 2 sessions · 17 changes");
                    assert_eq!(app.runtime.view.as_ref(), Some(&root));
                    assert!(!app.runtime.plan_view);
                    assert!(!app.runtime.ungrouped);
                    assert_eq!(app.runtime.active.as_deref(), Some("main"));
                    assert_eq!(app.runtime.visible_sessions(), [1, 2]);
                    let (items, output) = render(app, ctx, width, vec![]);
                    let repos_visible = items.iter().any(|(name, _)| name == "repo-1");
                    output.drop_without_applying_deltas();
                    if !repos_visible {
                        click(app, ctx, width, "Tabs");
                    }
                    click(app, ctx, width, "repo-1");
                    render(app, ctx, width, vec![]).1.drop_without_applying_deltas();
                    click(app, ctx, width, "repo-7 · 0 sessions · 0 changes");
                    assert_eq!(app.runtime.view.as_deref(), Some(Path::new("/project/repo-7")));
                    assert!(app.runtime.active.is_none());
                    assert_eq!(app.runtime.diff_root.as_deref(), Some(Path::new("/project/repo-7")));
                    if width == 1700.0 {
                        let (items, output) = render(app, ctx, width, vec![]);
                        assert!(items.iter().any(|(name, _)| name == "repo-7"));
                        output.drop_without_applying_deltas();
                    }
                }
            },
        );
    }

    #[test]
    fn overflow_tabs_and_actions_remain_clickable() {
        with_app("app::topbar::tests::overflow_tabs_and_actions_remain_clickable", |app, ctx| {
            fixture(app, 6);
            app.runtime.toast = None;
            app.runtime.config_error = None;
            app.runtime.hook_error = None;
            for width in [480.0, 550.0, 700.0] {
                fixture(app, 6);
                app.client.new_dialog = None;
                let collapsed = width < 700.0;
                let was_open = app.runtime.diff_open;
                if collapsed {
                    click(app, ctx, width, "Actions");
                }
                click(app, ctx, width, "diff");
                assert_ne!(app.runtime.diff_open, was_open, "width={width}: diff action");
                click(app, ctx, width, "Tabs");
                click(app, ctx, width, "Plan 0:");
                assert_eq!(app.runtime.selected_plan, 0, "width={width}: Tabs selection");
                assert!(app.runtime.plan_view);
                if collapsed {
                    click(app, ctx, width, "Actions");
                }
                click(app, ctx, width, "+ New");
                assert!(app.client.new_dialog.is_some(), "width={width}: new action");
                if collapsed {
                    click(app, ctx, width, "Actions");
                }
                let panel_was_open = app.runtime.panel == PanelMode::Plan && app.runtime.diff_open;
                click(app, ctx, width, "Plan");
                assert_ne!(
                    app.runtime.panel == PanelMode::Plan && app.runtime.diff_open,
                    panel_was_open,
                    "width={width}: plan action"
                );
                for (action, entry) in [("Project", "Open project…"), ("Settings", "Edit config")] {
                    if collapsed {
                        click(app, ctx, width, "Actions");
                    }
                    click(app, ctx, width, action);
                    // A submenu sizes before its first paint; inspect the settled pass.
                    render(app, ctx, width, vec![]).1.drop_without_applying_deltas();
                    let (_, output) = render(app, ctx, width, vec![]);
                    assert!(
                        output
                            .shapes
                            .iter()
                            .any(|shape| matches!(&shape.shape, egui::Shape::Text(t) if t.galley.text() == entry)),
                        "width={width}: {action} menu did not open"
                    );
                    output.drop_without_applying_deltas();
                    render(
                        app,
                        ctx,
                        width,
                        vec![egui::Event::Key {
                            key: Key::Escape,
                            physical_key: None,
                            pressed: true,
                            repeat: false,
                            modifiers: egui::Modifiers::NONE,
                        }],
                    )
                    .1
                    .drop_without_applying_deltas();
                }
            }
        });
    }

    #[test]
    fn the_remote_indicator_warns_while_the_operation_journal_cannot_be_written() {
        with_app(
            "app::topbar::tests::the_remote_indicator_warns_while_the_operation_journal_cannot_be_written",
            |app, ctx| {
                use crate::protocol::journal::Journal;
                use crate::protocol::rendezvous::{Phase, StatusCell};
                let status = StatusCell::new(|| {});
                status.update(|status| status.phase = Phase::Online);
                app.runtime.set_remote_view(crate::host::RemoteView {
                    enabled: true,
                    rendezvous: Some(status),
                    ..Default::default()
                });
                let label = |app: &mut Desktop<'_>| {
                    let (_, output) = render(app, ctx, 2400.0, vec![]);
                    let label = output.shapes.iter().find_map(|shape| match &shape.shape {
                        egui::Shape::Text(t) if t.galley.text().starts_with("Remote") => {
                            Some(t.galley.text().to_string())
                        }
                        _ => None,
                    });
                    output.drop_without_applying_deltas();
                    label.expect("the Remote indicator")
                };
                assert_eq!(label(app), "Remote", "online");
                app.runtime.ops = Journal::unwritable("No space left on device".into());
                assert_eq!(label(app), "Remote ●", "online, but clients cannot change anything");
                assert!(app.runtime.journal_problem().unwrap().contains("No space left on device"));
            },
        );
    }
}
