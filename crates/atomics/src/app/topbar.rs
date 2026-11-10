use super::*;

pub(super) use atomics_ui::topbar::record_item;

impl Desktop<'_> {
    /// The ATOMICS VM on a Mac with its bundle installed (docs/nixos-vm.md): "VM" with its state, and a
    /// menu that starts or stops it. Agents of this window run on this Mac unless the config's agents
    /// are the bundle's wrappers; `atomics --connect vm` runs the window on the VM's host instead.
    fn vm_indicator(&mut self, ui: &mut egui::Ui, in_menu: bool) {
        use crate::vm::Status;
        if ui.ctx().input(|input| input.viewport().minimized.unwrap_or(false)) {
            return;
        }
        let Some(monitor) = self.runtime.vm.clone() else { return };
        let Some(view) = monitor.view() else { return };
        ui.ctx().request_repaint_after(std::time::Duration::from_secs(5));
        let palette = self.palette();
        let text = view.status.indicator(view.busy);
        let color = match (&view.status, view.busy) {
            (_, Some(_)) | (Status::Starting | Status::Stopped, _) => palette.muted,
            (Status::Running, _) => palette.success,
            (Status::NotSetUp | Status::Unknown(_), _) => palette.warning,
        };
        let state = match view.busy {
            Some(busy) => busy.to_string(),
            None => view.status.label().to_string(),
        };
        let label = RichText::new(text).color(color);
        let items = |ui: &mut egui::Ui| {
            let confirm_id = ui.scope_id().with(("stop-vm", &view.instance));
            ui.label(RichText::new(format!("ATOMICS VM {}: {state}", view.instance)).strong());
            if let Some(error) = &view.error {
                ui.label(RichText::new(error).color(palette.warning));
            }
            let run = match view.status {
                Status::NotSetUp => {
                    ui.label("Set it up from a terminal: atomics vm setup --from <build folder>");
                    None
                }
                Status::Stopped | Status::Unknown(_) if view.busy.is_none() => {
                    ui.button("Start").clicked().then_some(true)
                }
                Status::Running if view.busy.is_none() => {
                    if ui.button("Stop").on_hover_text("Agents running in the VM stop with it").clicked() {
                        ui.data_mut(|data| data.insert_temp(confirm_id, true));
                    }
                    None
                }
                _ => None,
            };
            if ui.data(|data| data.get_temp::<bool>(confirm_id).unwrap_or(false)) {
                ui.label("Stop the VM? Agents running in it stop with it.");
                if ui.button("Stop VM").clicked() {
                    ui.data_mut(|data| data.remove::<bool>(confirm_id));
                    monitor.set_running(false);
                    ui.close();
                }
                if ui.button("Cancel").clicked() {
                    ui.data_mut(|data| data.remove::<bool>(confirm_id));
                }
            }
            if let Some(run) = run {
                monitor.set_running(run);
                ui.close();
            }
        };
        // Folded into the Actions menu where the bar has no room for it, as Settings is.
        let (response, _) = if in_menu || egui::containers::menu::is_in_menu(ui) {
            egui::containers::menu::SubMenuButton::new(label).ui(ui, items)
        } else {
            egui::containers::menu::MenuButton::new(label).ui(ui, items)
        };
        record_item(ui, "VM", &response);
        response.on_hover_text(format!("The ATOMICS VM {} is {state}", view.instance));
    }

    /// This host's items after `+ New`: Sign in or Reconnect while the ticket system needs it, the workspace,
    /// the GitHub counter, Remote, the VM, and in the Actions menu (`in_menu`) the ticket system's sign-in
    /// entries. The Tickets panel is the view switcher's (`atomics_ui::switcher`).
    pub(super) fn host_top_bar_items(&mut self, ui: &mut egui::Ui, in_menu: bool) {
        // The shared items: Sign in or Reconnect while the ticket system needs it, and the workspace.
        self.draw(|backend, client| atomics_ui::signin::top_bar_item(ui, backend, client));
        self.draw(|backend, client| atomics_ui::topbar::workspace_menu(ui, backend, client));
        self.github_counter(ui, in_menu);

        self.vm_indicator(ui, in_menu);
        if in_menu && let Some(server) = self.runtime.sign_server() {
            let system = self.runtime.ticket_config().map(|c| c.system.clone()).unwrap_or_default();
            let signed_in = self
                .runtime
                .sign_in_info()
                .is_some_and(|info| info.state == atomics_core::projection::SignInState::SignedIn);
            if signed_in && ui.button(format!("Tickets: sign out of {system}")).clicked() {
                self.runtime.start_sign_out();
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
        render_at(app, ctx, width, None, events)
    }

    /// [`render`] at `time`, for what shows only after a while, such as a hover's text.
    fn render_at(
        app: &mut Desktop<'_>,
        ctx: &egui::Context,
        width: f32,
        time: Option<f64>,
        events: Vec<egui::Event>,
    ) -> (Vec<(String, Rect)>, egui::FullOutput) {
        ctx.data_mut(|d| d.insert_temp(egui::Id::NULL.with("topbar-items"), Vec::<(String, Rect)>::new()));
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(width, 800.0))),
                time,
                events,
                ..Default::default()
            },
            |ui| {
                egui::Panel::top("top")
                    .exact_size(34.0)
                    .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(10, 4)))
                    .show(ui, |ui| app.top_bar(ui));
                app.draw(|backend, client| atomics_ui::management::dialogs(ui.ctx(), backend, client));
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
        app.runtime.resources.load = Some(123.4);
    }

    #[test]
    fn the_bar_shows_the_open_workspace_and_reconnect_and_both_work() {
        with_app("app::topbar::tests::the_bar_shows_the_open_workspace_and_reconnect_and_both_work", |app, ctx| {
            let dir = atomics_core::workspace::workspaces_dir();
            std::fs::create_dir_all(&dir).unwrap();
            for name in ["team", "daily"] {
                std::fs::write(dir.join(format!("{name}.toml")), "projects = []\n").unwrap();
            }
            app.runtime.workspace = Some("team".into());
            let fake = crate::mcp_client::FakeOAuthServer::start();
            let (_keys, oauth) = crate::mcp_client::oauth_test_machine();
            app.runtime.config.tickets = toml::from_str("system = \"tracker\"").unwrap();
            app.runtime.config.tickets.server = fake.server();
            app.runtime.tickets.sign.override_oauth(oauth);
            app.runtime.tickets.sign.headless = true;
            app.runtime.apply_ticket_config();
            app.runtime.poll_signin();
            app.runtime.ticket_job_failed("sign-in required again: the server refused the saved sign-in");
            app.runtime.poll_signin();
            app.runtime.toast = None;

            // In a narrower window they fold into the Actions menu beside the centered pill, and show there.
            let (items, output) = render(app, ctx, 1100.0, vec![]);
            output.drop_without_applying_deltas();
            assert!(items.iter().any(|(item, _)| item == "Actions"), "{items:?}");
            assert!(!items.iter().any(|(item, _)| item == "Workspace" || item == "sign-in"), "{items:?}");
            click(app, ctx, 1100.0, "Actions");
            render(app, ctx, 1100.0, vec![]).1.drop_without_applying_deltas();
            let (_, output) = render(app, ctx, 1100.0, vec![]);
            let menu: Vec<String> = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                    _ => None,
                })
                .collect();
            output.drop_without_applying_deltas();
            for label in ["Reconnect", "Workspace: team"] {
                assert!(menu.iter().any(|text| text == label), "{label} in the Actions menu: {menu:?}");
            }
            render(
                app,
                ctx,
                1100.0,
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

            // With room, right of the pill; the switcher's Tickets segment shows the sign-in it waits for.
            let (items, output) = render(app, ctx, 2400.0, vec![]);
            let tickets = items.iter().find(|(name, _)| name == "view:Tickets").expect("the Tickets segment").1;
            assert!(
                output.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Text(text)
                    if text.galley.text() == "●" && tickets.contains(text.visual_bounding_rect().center()))),
                "a sign-in dot on the Tickets segment"
            );
            output.drop_without_applying_deltas();
            for name in ["Workspace", "sign-in", "chief"] {
                assert!(items.iter().any(|(item, _)| item == name), "{name}: {items:?}");
            }
            let chief = items.iter().find(|(name, _)| name == "chief").unwrap().1;
            for (name, rect) in items.iter().filter(|(name, _)| name == "Workspace" || name == "sign-in") {
                assert!(rect.min.x >= chief.max.x, "{name} is right of the chief's pill: {items:?}");
            }

            // Reconnect starts the sign-in and opens the panel that shows its steps.
            click(app, ctx, 2400.0, "sign-in");
            assert!(app.runtime.tickets.sign.job.is_some());
            assert!(app.runtime.panel == PanelMode::Tickets && app.runtime.diff_open);
            let _ = app.runtime.cancel_sign_in();

            // The workspace item names the open one and opens another.
            click(app, ctx, 2400.0, "Workspace");
            click(app, ctx, 2400.0, "daily");
            assert_eq!(app.runtime.workspace.as_deref(), Some("daily"));
            let (items, output) = render(app, ctx, 2400.0, vec![]);
            let labels: Vec<String> = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                    _ => None,
                })
                .collect();
            output.drop_without_applying_deltas();
            assert!(labels.iter().any(|label| label == "Workspace: daily"), "{labels:?}; {items:?}");
        });
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
                        // The view switcher stays in the bar when the actions fold; it drops its names.
                        let required: &[&str] = if collapsed {
                            &["chief", "Actions", "view:Board", "view:Diff"]
                        } else {
                            &["chief", "Settings", "Project", "+ New", "view:Board", "view:Diff"]
                        };
                        for &name in required {
                            assert_eq!(items.iter().filter(|(n, _)| n == name).count(), 1, "{case}: {name}");
                            if name != "chief" && !name.starts_with("view:") {
                                let width = items.iter().find(|(n, _)| n == name).unwrap().1.width();
                                let natural = action_widths.entry(name).or_insert(width);
                                assert!((width - *natural).abs() < 0.5, "{case}: {name} shrank");
                            }
                        }
                        assert_eq!(
                            items.iter().filter(|(name, _)| name == "Boards").count(),
                            1,
                            "{case}: the boards and whatever folded into their menu stay reachable"
                        );
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
                        let chief = items.iter().find(|(n, _)| n == "chief").unwrap().1;
                        assert!((chief.center().x - width / 2.0).abs() < 1.0, "{case}: the pill is centered");
                        if width == 5120.0 {
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
    fn agent_counts_separate_working_and_open_and_collapse_together() {
        with_app("app::topbar::tests::agent_counts_separate_working_and_open_and_collapse_together", |app, ctx| {
            fixture(app, 3);
            app.runtime.sessions[0].status = AgentStatus::Working;
            let root = Path::new("/project/ATOMICS");
            for i in 0..12 {
                app.runtime.sessions.push(session(
                    &format!("agent-{i}"),
                    AgentKind::Generic,
                    if i < 2 { AgentStatus::Working } else { AgentStatus::Done },
                    root,
                ));
            }
            app.runtime.sessions.push(session("shell", AgentKind::Shell, AgentStatus::Working, root));
            for working in [true, false] {
                if !working {
                    for session in app.runtime.sessions.iter_mut().filter(|s| !s.chief) {
                        session.status = AgentStatus::Idle;
                    }
                }
                let expected = if working { "2 working · 12 open" } else { "12 idle" };
                for width in [480.0, 550.0, 700.0, 900.0, 1200.0, 1500.0, 2400.0, 5120.0] {
                    let (items, output) = render(app, ctx, width, vec![]);
                    let counts_shown = items.iter().any(|(name, _)| name == "agents");
                    assert_eq!(
                        output.shapes.iter().any(|shape| matches!(&shape.shape,
                        egui::Shape::Text(text) if text.galley.text() == expected)),
                        counts_shown,
                        "width={width}"
                    );
                    if width == 5120.0 {
                        assert!(counts_shown);
                    }
                    if width == 480.0 {
                        assert!(!counts_shown);
                    }
                    output.drop_without_applying_deltas();
                }
            }
        });
    }

    #[test]
    fn the_count_says_idle_only_when_every_agent_is() {
        with_app("app::topbar::tests::the_count_says_idle_only_when_every_agent_is", |app, ctx| {
            let root = Path::new("/project/ATOMICS");
            let cases: [(&[AgentStatus], &str); 7] = [
                (&[AgentStatus::Idle, AgentStatus::Idle], "2 idle"),
                (&[AgentStatus::Working, AgentStatus::NeedsInput], "1 working · 2 open"),
                (&[AgentStatus::NeedsInput, AgentStatus::Idle], "2 open"),
                (&[AgentStatus::NeedsInput, AgentStatus::NeedsInput], "2 open"),
                (&[AgentStatus::Done, AgentStatus::Idle], "2 open"),
                (&[AgentStatus::Exited, AgentStatus::Idle], "2 open"),
                (&[AgentStatus::NeedsInput, AgentStatus::Done, AgentStatus::Exited, AgentStatus::Idle], "4 open"),
            ];
            for (statuses, expected) in cases {
                app.runtime.sessions = statuses
                    .iter()
                    .enumerate()
                    .map(|(i, status)| session(&format!("agent-{i}"), AgentKind::Generic, *status, root))
                    .collect();
                let (_, output) = render(app, ctx, 5120.0, vec![]);
                let (shown, count) =
                    output.shapes.iter().fold((false, None), |(shown, count), shape| match &shape.shape {
                        egui::Shape::Text(text) if text.galley.text() == expected => (true, count),
                        egui::Shape::Text(text) if text.galley.text().ends_with(" idle") => {
                            (shown, Some(text.galley.text().to_string()))
                        }
                        _ => (shown, count),
                    });
                output.drop_without_applying_deltas();
                assert!(shown, "{statuses:?}: expected {expected}, the bar says {count:?}");
                assert_eq!(count, None, "{statuses:?}: agents that are not idle do not read as idle");
            }
        });
    }

    #[test]
    fn the_chief_keeps_its_place_whatever_it_and_the_plans_do() {
        with_app("app::topbar::tests::the_chief_keeps_its_place_whatever_it_and_the_plans_do", |app, ctx| {
            for width in [480.0, 700.0, 1100.0, 1600.0, 2400.0] {
                let mut places = Vec::new();
                for (count, status, heartbeat, open, toast) in [
                    (0, None, false, false, false),
                    (0, Some(AgentStatus::Starting), false, true, false),
                    (3, Some(AgentStatus::Working), true, true, true),
                    (6, Some(AgentStatus::NeedsInput), true, false, true),
                    (6, Some(AgentStatus::Done), false, true, false),
                ] {
                    fixture(app, count);
                    match status {
                        Some(status) => app.runtime.sessions[0].status = status,
                        None => app.runtime.sessions.clear(),
                    }
                    if !heartbeat {
                        app.runtime.heartbeat.last = None;
                    }
                    if !toast {
                        app.runtime.toast = None;
                    }
                    app.runtime.chief_open = open;
                    // Twice: the first frame of a menu or a tab measures itself.
                    render(app, ctx, width, vec![]).1.drop_without_applying_deltas();
                    let (items, output) = render(app, ctx, width, vec![]);
                    output.drop_without_applying_deltas();
                    let chief = items.iter().find(|(n, _)| n == "chief").unwrap().1;
                    places.push((chief, format!("plans={count} status={status:?} heartbeat={heartbeat}")));
                }
                for (rect, case) in &places[1..] {
                    assert_eq!(*rect, places[0].0, "width={width}: the pill moved with {case}");
                }
            }
        });
    }

    #[test]
    fn the_view_switcher_shows_each_view_with_its_count_and_the_window_remembers_the_choice() {
        with_app(
            "app::topbar::tests::the_view_switcher_shows_each_view_with_its_count_and_the_window_remembers_the_choice",
            |app, ctx| {
                fixture(app, 3);
                app.runtime.toast = None;
                let plan = app.runtime.selected_plan;
                app.runtime.plans[plan].tasks[0].status = TaskStatus::Review;
                let shown = |app: &mut Desktop<'_>| {
                    render(app, ctx, 2400.0, vec![]).1.drop_without_applying_deltas();
                    let (items, output) = render(app, ctx, 2400.0, vec![]);
                    let text: Vec<String> = output
                        .shapes
                        .iter()
                        .filter_map(|s| {
                            if let egui::Shape::Text(t) = &s.shape { Some(t.galley.text().into()) } else { None }
                        })
                        .collect();
                    output.drop_without_applying_deltas();
                    (items.into_iter().map(|(name, _)| name).collect::<Vec<_>>(), text)
                };
                let (items, text) = shown(app);
                assert!(items.contains(&"view:Board".into()) && items.contains(&"view:Diff".into()), "{items:?}");
                assert!(!items.contains(&"view:Tickets".into()), "no Tickets without a ticket system: {items:?}");
                assert!(
                    text.contains(&"Board 0/1".into()) && text.contains(&"1".into()),
                    "the task in review: {text:?}"
                );
                for old in ["Show diff", "Hide diff", "Plan 0/1", "Tickets"] {
                    assert!(!text.contains(&old.into()), "{old} is the switcher's now: {text:?}");
                }
                app.runtime.tickets.config = Some(std::sync::Arc::new(atomics_core::tickets::TicketConfig {
                    system: "demo".into(),
                    ..Default::default()
                }));
                assert!(shown(app).0.contains(&"view:Tickets".into()));
                app.runtime.diff_open = false;
                click(app, ctx, 2400.0, "view:Tickets");
                assert!(app.runtime.panel == PanelMode::Tickets && app.runtime.diff_open);
                assert_eq!(app.runtime.store.panel_view.as_deref(), Some("tickets"), "the window remembers it");
                assert_eq!(Store::load(&paths::store_file()).panel_view.as_deref(), Some("tickets"));
                // Another plan's tab keeps the view; the editor counts as the diff's.
                app.send(Command::ShowPlan(0));
                assert!(app.runtime.panel == PanelMode::Tickets && app.runtime.diff_open);
                app.runtime.panel = PanelMode::Editor;
                click(app, ctx, 2400.0, "view:Diff");
                assert!(
                    app.runtime.panel == PanelMode::Editor && !app.runtime.diff_open,
                    "the diff's segment hides it"
                );
                click(app, ctx, 2400.0, "view:Diff");
                assert!(app.runtime.panel == PanelMode::Editor && app.runtime.diff_open, "and shows the editor again");
            },
        );
    }

    #[test]
    fn the_dropdown_counts_the_boards_that_wait_and_lists_them_first_at_supported_widths() {
        with_app(
            "app::topbar::tests::the_dropdown_counts_the_boards_that_wait_and_lists_them_first_at_supported_widths",
            |app, ctx| {
                fixture(app, 5);
                // Running, approved; blocked, approved; not approved yet; and two finished.
                app.runtime.plans[0].status = PlanStatus::Open;
                app.runtime.plans[0].approved = true;
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
                app.runtime.plans[1].approved = true;
                app.runtime.plans[1].goal = "A needs-you plan with a long descriptive name".into();
                app.runtime.plans[1].tasks[0].status = TaskStatus::Blocked;
                app.runtime.plans[2].goal = "An unapproved plan".into();
                app.runtime.plans[3].status = PlanStatus::Completed;
                app.runtime.plans[4].status = PlanStatus::Completed;
                app.runtime.plan_view = false;
                for id in ["first", "second"] {
                    app.runtime.sessions.push(session(
                        id,
                        AgentKind::Generic,
                        AgentStatus::Working,
                        Path::new("/project/ATOMICS"),
                    ));
                }
                app.runtime.toast = None;
                app.runtime.changed_plans.insert(app.runtime.plans[0].id.clone());
                let warning = app.palette().warning;
                for width in [480.0, 900.0, 1200.0, 1400.0] {
                    let (items, output) = render(app, ctx, width, vec![]);
                    let boards = items.iter().find(|(name, _)| name == "Boards").expect("the dropdown").1;
                    let in_it = |text: &&egui::epaint::TextShape| boards.contains(text.visual_bounding_rect().center());
                    let labels: Vec<_> = output
                        .shapes
                        .iter()
                        .filter_map(|shape| if let egui::Shape::Text(text) = &shape.shape { Some(text) } else { None })
                        .collect();
                    // The count, in the color of what needs the user, and the word: no plan's name yet.
                    assert!(
                        labels.iter().copied().filter(in_it).any(|text| text.galley.text() == "2"
                            && text.galley.job.sections.iter().any(|section| section.format.color == warning)),
                        "width={width}: the count"
                    );
                    assert!(
                        labels.iter().copied().filter(in_it).any(|text| text.galley.text() == "Boards"),
                        "width={width}"
                    );
                    for finished in ["Plan 3:", "Plan 4:"] {
                        assert!(
                            !labels.iter().any(|text| text.galley.text().starts_with(finished)),
                            "width={width}: {finished} left the bar"
                        );
                    }
                    output.drop_without_applying_deltas();

                    click(app, ctx, width, "Boards");
                    render(app, ctx, width, vec![]).1.drop_without_applying_deltas();
                    let (_, output) = render(app, ctx, width, vec![]);
                    let labels: Vec<_> = output
                        .shapes
                        .iter()
                        .filter_map(|shape| if let egui::Shape::Text(text) = &shape.shape { Some(text) } else { None })
                        .collect();
                    let at = |label: &str| {
                        labels
                            .iter()
                            .rposition(|text| text.galley.text().starts_with(label))
                            .unwrap_or_else(|| panic!("width={width}: {label}"))
                    };
                    let order = [
                        at("NEEDS YOU · 2"),
                        at("A needs-you plan"),
                        at("An unapproved plan"),
                        at("ACTIVE · 1"),
                        at("A long running plan"),
                        at("History"),
                    ];
                    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "width={width}: {order:?}");
                    for state in ["needs you", "needs approval", "2 running"] {
                        let text = labels[at(state)];
                        assert!(!text.galley.elided, "width={width}: {state} was cut");
                    }
                    output.drop_without_applying_deltas();
                    // Nothing is left of it after Escape; the menu is not in the next test's way.
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
            },
        );
    }

    #[test]
    fn every_button_in_the_bar_has_one_height() {
        with_app("app::topbar::tests::every_button_in_the_bar_has_one_height", |app, ctx| {
            fixture(app, 3);
            for width in [700.0, 2400.0] {
                render(app, ctx, width, vec![]).1.drop_without_applying_deltas();
                let (items, output) = render(app, ctx, width, vec![]);
                output.drop_without_applying_deltas();
                let controls: Vec<_> = items
                    .iter()
                    .filter(|(name, _)| {
                        ["Boards", "+ New", "Project", "Settings", "Remote", "Actions"].contains(&name.as_str())
                            || name.starts_with("All ·")
                            || name.starts_with("Ungrouped ·")
                            || name.starts_with("Repos ·")
                            || name.starts_with("view:")
                    })
                    .collect();
                assert!(controls.len() >= 3, "width={width}: {items:?}");
                for (name, rect) in controls {
                    assert_eq!(rect.height(), atomics_ui::theme::CONTROL_HEIGHT, "width={width}: {name}");
                }
            }
        });
    }

    #[test]
    fn the_button_names_the_board_on_screen_and_gives_it_up_in_a_narrow_bar() {
        with_app(
            "app::topbar::tests::the_button_names_the_board_on_screen_and_gives_it_up_in_a_narrow_bar",
            |app, ctx| {
                fixture(app, 3);
                app.runtime.toast = None;
                app.runtime.config_error = None;
                app.runtime.hook_error = None;
                // Short enough for the button at its widest.
                let goal = "Plan 2: resource budgets and approvals";
                app.runtime.plans[2].goal = goal.into();
                let name_of = |output: &egui::FullOutput, boards: Rect| {
                    output.shapes.iter().find_map(|shape| match &shape.shape {
                        egui::Shape::Text(text)
                            if text.galley.text() == goal && boards.contains(text.visual_bounding_rect().center()) =>
                        {
                            Some(text.galley.elided)
                        }
                        _ => None,
                    })
                };
                let wide = render(app, ctx, 5120.0, vec![]);
                let boards = wide.0.iter().find(|(name, _)| name == "Boards").unwrap().1;
                assert_eq!(name_of(&wide.1, boards), Some(false), "room for the whole name");
                wide.1.drop_without_applying_deltas();
                let narrow = render(app, ctx, 700.0, vec![]);
                let boards = narrow.0.iter().find(|(name, _)| name == "Boards").unwrap().1;
                assert_eq!(name_of(&narrow.1, boards), Some(true), "it shortens rather than leaves the bar");
                narrow.1.drop_without_applying_deltas();
            },
        );
    }

    #[test]
    fn a_long_goal_keeps_the_button_narrow_in_a_wide_bar_and_shows_whole_on_hover() {
        with_app(
            "app::topbar::tests::a_long_goal_keeps_the_button_narrow_in_a_wide_bar_and_shows_whole_on_hover",
            |app, ctx| {
                fixture(app, 3);
                app.runtime.toast = None;
                app.runtime.config_error = None;
                app.runtime.hook_error = None;
                let goal =
                    format!("Plan 2: {}", "resource budgets and built-in approval rules for every agent, ".repeat(8));
                let goal = goal.trim_end().to_string();
                app.runtime.plans[2].goal = goal.clone();
                let texts = |output: &egui::FullOutput| -> Vec<(bool, Pos2)> {
                    output
                        .shapes
                        .iter()
                        .filter_map(|shape| match &shape.shape {
                            egui::Shape::Text(text) if text.galley.text().starts_with(&goal) => {
                                Some((text.galley.elided, text.visual_bounding_rect().center()))
                            }
                            _ => None,
                        })
                        .collect()
                };
                // A window about 3750 points wide: the button stays near 360 points, the goal cut short in it.
                let width = 3750.0;
                let (items, output) = render_at(app, ctx, width, Some(0.0), vec![]);
                let boards = items.iter().find(|(name, _)| name == "Boards").unwrap().1;
                assert!(boards.width() <= 361.0, "{boards:?}");
                let shown = texts(&output);
                assert!(shown.iter().any(|&(elided, at)| elided && boards.contains(at)), "{shown:?}");
                output.drop_without_applying_deltas();
                // Hovering it shows the whole goal.
                for (time, events) in [(1.0, vec![egui::Event::PointerMoved(boards.center())]), (2.0, vec![])] {
                    render_at(app, ctx, width, Some(time), events).1.drop_without_applying_deltas();
                }
                let (_, output) = render_at(app, ctx, width, Some(3.0), vec![]);
                let shown = texts(&output);
                assert!(shown.iter().any(|&(elided, at)| !elided && !boards.contains(at)), "{shown:?}");
                output.drop_without_applying_deltas();
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
                        click(app, ctx, width, "Boards");
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
                        click(app, ctx, width, "Boards");
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
    fn boards_and_folded_actions_remain_clickable() {
        with_app("app::topbar::tests::boards_and_folded_actions_remain_clickable", |app, ctx| {
            fixture(app, 6);
            app.runtime.toast = None;
            app.runtime.config_error = None;
            app.runtime.hook_error = None;
            for width in [480.0, 550.0, 700.0, 1200.0, 1700.0] {
                fixture(app, 6);
                app.client.new_dialog = None;
                let (items, output) = render(app, ctx, width, vec![]);
                output.drop_without_applying_deltas();
                let collapsed = items.iter().any(|(name, _)| name == "Actions");
                assert_eq!(collapsed, width < 1700.0, "width={width}: the actions fold beside a centered pill");
                // The switcher's segments are in the bar at every width: one shows its view, again hides it.
                for (segment, view) in [("view:Diff", PanelMode::Diff), ("view:Board", PanelMode::Plan)] {
                    app.runtime.panel = PanelMode::Tickets;
                    app.runtime.diff_open = true;
                    click(app, ctx, width, segment);
                    assert!(app.runtime.panel == view && app.runtime.diff_open, "width={width}: {segment} shows");
                    click(app, ctx, width, segment);
                    assert!(app.runtime.panel == view && !app.runtime.diff_open, "width={width}: {segment} hides");
                }
                click(app, ctx, width, "Boards");
                click(app, ctx, width, "Plan 0:");
                assert_eq!(app.runtime.selected_plan, 0, "width={width}: a board's row picks it");
                assert!(app.runtime.plan_view);
                if collapsed {
                    click(app, ctx, width, "Actions");
                }
                click(app, ctx, width, "+ New");
                assert!(app.client.new_dialog.is_some(), "width={width}: new action");
                for (action, entry) in [("Project", "Open project…"), ("Settings", "Where agents run")] {
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
                    app.client.ui.management.settings = false;
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
