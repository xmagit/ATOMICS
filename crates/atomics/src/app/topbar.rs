use super::*;

/// Measure with the same fonts, padding and widgets that will actually be drawn.
/// The disabled, invisible child cannot activate controls or take space from the bar.
pub(super) fn measure(ui: &mut egui::Ui, contents: impl FnOnce(&mut egui::Ui)) -> f32 {
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("topbar-measure")
            .max_rect(Rect::from_min_size(ui.max_rect().min, vec2(100_000.0, ui.max_rect().height())))
            .layout(egui::Layout::left_to_right(egui::Align::Center))
            .sizing_pass()
            .invisible(),
    );
    contents(&mut child);
    child.min_rect().width().ceil()
}

/// A slot bounds both painting and layout. Popups retain their own clip rects.
fn slot(ui: &mut egui::Ui, id: &str, rect: Rect, contents: impl FnOnce(&mut egui::Ui)) {
    let mut child = ui.new_child(
        egui::UiBuilder::new().id_salt(id).max_rect(rect).layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.set_clip_rect(ui.clip_rect().intersect(rect));
    child.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
    contents(&mut child);
}

pub(super) fn record_item(_ui: &egui::Ui, _name: &str, _response: &egui::Response) {
    #[cfg(test)]
    if !_ui.is_sizing_pass() {
        _ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<Vec<(String, Rect)>>(egui::Id::NULL.with("topbar-items"))
                .push((_name.into(), _response.rect));
        });
    }
}

impl Desktop<'_> {
    pub(super) fn top_bar(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        let gap = ui.spacing().item_spacing.x;
        let controls = measure(ui, |ui| self.top_bar_controls(ui, false, false));
        let controls_with_load = measure(ui, |ui| self.top_bar_controls(ui, true, false));
        let full_chief = measure(ui, |ui| self.chief_pill(ui, false));
        let compact_chief = measure(ui, |ui| self.chief_pill(ui, true));
        let full_brand = measure(ui, |ui| self.top_bar_brand(ui, true));
        let compact_brand = measure(ui, |ui| self.top_bar_brand(ui, false));
        let tabs = measure(ui, |ui| self.view_tabs(ui));
        let (urgent_label, urgent_color) = self.urgent_tabs_label();
        let min_tabs = measure(ui, |ui| {
            ui.menu_button(RichText::new(&urgent_label).color(urgent_color), |_| {});
        });
        let protected_navigation = self.urgent_navigation_width(ui).max(min_tabs);

        // Once even the minimum navigation cannot fit, put the actions in a
        // menu. All four slots then retain their measured natural minimums.
        let actions_menu = compact_brand + protected_navigation + compact_chief + controls + gap * 3.0 > rect.width();
        let show_load =
            !actions_menu && full_brand + tabs + full_chief + controls_with_load + gap * 3.0 <= rect.width();
        let controls_width = if actions_menu {
            measure(ui, |ui| {
                ui.menu_button("Actions", |_| {});
            })
        } else if show_load {
            controls_with_load
        } else {
            controls
        };
        let show_agents =
            full_brand + protected_navigation + compact_chief + controls_width + gap * 3.0 <= rect.width();
        let brand_width = if show_agents { full_brand } else { compact_brand };
        let compact = brand_width + protected_navigation + full_chief + controls_width + gap * 3.0 > rect.width();
        let chief_width = if compact { compact_chief } else { full_chief };
        let controls_left = rect.right() - controls_width;
        let tabs_left = rect.left() + brand_width + gap;
        // Keep the wide-screen center when navigation fits. Otherwise use the
        // free space up to the actions before deciding which tabs overflow.
        let chief_limit = controls_left - gap - chief_width;
        let desired_navigation = tabs_left + tabs + gap;
        let navigation_goal = desired_navigation.min(chief_limit);
        let chief_left =
            (rect.center().x - chief_width / 2.0).max(navigation_goal).max(tabs_left + min_tabs + gap).min(chief_limit);
        let navigation = Rect::from_x_y_ranges(tabs_left..=chief_left - gap, rect.y_range());
        slot(ui, "brand", rect.with_max_x(rect.left() + brand_width), |ui| self.top_bar_brand(ui, show_agents));
        slot(ui, "chief", rect.with_min_x(chief_left).with_max_x(chief_left + chief_width), |ui| {
            self.chief_pill(ui, compact);
        });
        slot(ui, "actions", rect.with_min_x(controls_left), |ui| {
            if actions_menu {
                let response = ui.menu_button("Actions", |ui| self.top_bar_controls(ui, false, true)).response;
                record_item(ui, "Actions", &response);
            } else {
                self.top_bar_controls(ui, show_load, false);
            }
        });
        slot(ui, "navigation", navigation, |ui| {
            self.view_tabs(ui);
            self.top_bar_notice(ui);
        });
        ui.allocate_rect(rect, Sense::hover());
    }

    fn top_bar_brand(&self, ui: &mut egui::Ui, show_agents: bool) {
        let response = ui.image((self.client.icon.id(), vec2(20.0, 20.0)));
        record_item(ui, "icon", &response);
        let response =
            ui.label(RichText::new("ATOMICS").strong().size(14.0).color(self.runtime.palette.accent).monospace());
        record_item(ui, "brand", &response);
        if show_agents {
            let agents = self.runtime.sessions.iter().filter(|s| s.is_agent()).count();
            let response = ui.label(
                RichText::new(format!("{agents} agent{}", if agents == 1 { "" } else { "s" }))
                    .color(self.runtime.palette.muted),
            );
            record_item(ui, "agents", &response);
            let names: HashSet<_> = self.runtime.sessions.iter().filter_map(|s| s.sandbox.as_ref()).collect();
            if !names.is_empty() {
                let sampler = &self.runtime.sandbox_usage;
                let label = if sampler.usage.is_empty() {
                    "VMs —".into()
                } else {
                    format!("VMs {}", sampler.total().label())
                };
                let response =
                    ui.label(RichText::new(label).size(11.0).color(self.runtime.palette.muted)).on_hover_text(format!(
                        "Host overhead of {}/{} sandbox VM process trees. 100% CPU = one core.{}",
                        sampler.usage.len(),
                        names.len(),
                        sampler.error.as_ref().map_or(String::new(), |e| format!("\n{e}"))
                    ));
                record_item(ui, "sandbox-total", &response);
            }
        }
        ui.separator();
    }

    fn top_bar_controls(&mut self, ui: &mut egui::Ui, show_load: bool, in_menu: bool) {
        let response = ui.button(if self.runtime.diff_open { "Hide diff" } else { "Show diff" });
        record_item(ui, "diff", &response);
        if response.on_hover_text(shortcut_label(ui.ctx(), &self.runtime.config.keys.toggle_diff)).clicked() {
            self.runtime.diff_open = !self.runtime.diff_open;
            if in_menu {
                ui.close();
            }
        }
        let response = ui.button("+ New");
        record_item(ui, "+ New", &response);
        if response.on_hover_text(shortcut_label(ui.ctx(), &self.runtime.config.keys.new_session)).clicked() {
            self.open_new_dialog(None);
            if in_menu {
                ui.close();
            }
        }
        let plan = &self.runtime.plans[self.runtime.selected_plan];
        let (done, total) = (plan.count(TaskStatus::Done), plan.tasks.len());
        let label = if total == 0 { "Plan".to_string() } else { format!("Plan {done}/{total}") };
        let attention = plan.count(TaskStatus::Review) + plan.count(TaskStatus::Blocked);
        let text = RichText::new(label).color(if attention > 0 {
            self.runtime.palette.warning
        } else {
            self.runtime.palette.foreground
        });
        let response = ui.selectable_label(self.runtime.panel == PanelMode::Plan && self.runtime.diff_open, text);
        record_item(ui, "Plan", &response);
        if response.on_hover_text(shortcut_label(ui.ctx(), &self.runtime.config.keys.toggle_plan)).clicked() {
            self.runtime.toggle_plan();
            if in_menu {
                ui.close();
            }
        }
        if self.runtime.ticket_config().is_some() {
            // A dot says the ticket system wants a sign-in.
            let signed_out = self.runtime.tickets.panel.signin;
            let text = RichText::new(if signed_out { "Tickets ●" } else { "Tickets" }).color(if signed_out {
                self.runtime.palette.warning
            } else {
                self.runtime.palette.foreground
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
        let response = ui.menu_button("Project", |ui| self.project_menu(ui)).response;
        record_item(ui, "Project", &response);
        if show_load {
            self.load_gauge(ui);
        }
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

    fn top_bar_notice(&mut self, ui: &mut egui::Ui) {
        if ui.available_width() < 24.0 {
            return;
        }
        let waiting = self.runtime.sessions.iter().filter(|s| s.status.needs_attention()).count();
        let mut notices = Vec::new();
        if waiting > 0 {
            notices.push(format!("● {waiting} need{} you", if waiting == 1 { "s" } else { "" }));
        }
        if let Some((text, _)) = &self.runtime.toast {
            notices.push(text.clone());
        }
        if let Some(err) = &self.runtime.config_error {
            notices.push(format!("config error: {err}"));
        }
        if let Some(err) = &self.runtime.theme_error {
            notices.push(format!("theme: {err}"));
        }
        if let Some(err) = &self.runtime.hook_error {
            notices.push(format!("hooks offline: {err}"));
        }
        if notices.is_empty() {
            return;
        }
        let text = notices.join(" · ");
        let response = ui.add(
            egui::Label::new(RichText::new(&text).color(if waiting > 0 {
                self.runtime.palette.warning
            } else {
                self.runtime.palette.foreground
            }))
            .truncate()
            .sense(Sense::click()),
        );
        record_item(ui, "notice", &response);
        if response.on_hover_text(text).clicked()
            && let Some(i) = self.runtime.sessions.iter().position(|s| s.status.needs_attention())
        {
            let id = self.runtime.sessions[i].id.clone();
            self.activate(&id, true);
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
                                .any(|section| section.format.color == app.runtime.palette.warning)
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
}
