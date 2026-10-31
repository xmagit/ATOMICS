use super::*;
use atomics_core::heartbeat::{Finding, Kind};

struct WakeConditions {
    wake_secs: u64,
    status: Option<AgentStatus>,
    work_open: bool,
    approval_pending: bool,
    input_age: Option<Duration>,
    input_empty: bool,
    elapsed: Option<Duration>,
}

impl WakeConditions {
    fn should_wake(&self) -> bool {
        self.wake_secs != 0
            && matches!(self.status, Some(AgentStatus::Idle | AgentStatus::Done))
            && self.work_open
            && !self.approval_pending
            && self.input_age.is_none_or(|age| age >= Duration::from_secs(60))
            && self.input_empty
            && self.elapsed.is_some_and(|elapsed| elapsed >= Duration::from_secs(self.wake_secs))
    }
}

/// Require a visible, empty prompt; an unknown screen is unsafe to type into.
/// Check continuation rows too, so a multiline draft with an empty first row is preserved.
fn chief_input_empty(screen: &str) -> bool {
    let lines: Vec<_> = screen.lines().collect();
    let Some((index, prompt)) = lines
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, line)| line.trim_start().strip_prefix(['❯', '›', '>']).map(|prompt| (i, prompt)))
    else {
        return false;
    };
    prompt.trim().is_empty()
        && lines[index + 1..]
            .iter()
            .take_while(|line| !line.trim_start().starts_with(['─', '━', '═', '╰']))
            .all(|line| line.trim().is_empty())
}

fn wake_message(events: usize, findings: &[Finding]) -> String {
    let kinds: Vec<_> = [
        (Kind::AgentGone, "agent_gone"),
        (Kind::Unreported, "unreported"),
        (Kind::Approval, "approval"),
        (Kind::HeldMessages, "held_messages"),
        (Kind::StuckWorking, "stuck_working"),
    ]
    .into_iter()
    .filter_map(|(kind, label)| findings.iter().any(|finding| finding.kind == kind).then_some(label))
    .collect();
    let kinds = if kinds.is_empty() { "none".into() } else { kinds.join(", ") };
    format!(
        "ATOMICS wake: {events} new events, {} heartbeat findings ({kinds}). Call atomics_wait and act; if nothing changed, post one short line.",
        findings.len()
    )
}

fn chief_work_open(plans: &[Plan], sessions: &[Session]) -> bool {
    plans.iter().any(|plan| {
        plan.is_open() && plan.tasks.iter().any(|task| matches!(task.status, TaskStatus::Running | TaskStatus::Review))
    }) || sessions.iter().any(|s| !s.chief && s.is_agent())
}

impl State {
    /// Runs on the host's background tick, including while the chief is hidden or no client is drawing.
    pub(super) fn wake_chief(&mut self) {
        if self.config.chief.wake_secs == 0 {
            return;
        }
        let Some(index) = self.sessions.iter().position(|s| s.chief) else { return };
        let chief = &self.sessions[index];
        if !matches!(chief.status, AgentStatus::Idle | AgentStatus::Done) {
            return;
        }
        let Some(terminal) = chief.terminal.as_ref().filter(|t| !t.exited()) else { return };
        // Another message must be picked up before adding an automatic one.
        if chief.pending.is_some() || !chief.held.is_empty() {
            return;
        }
        let screen = terminal.screen_text(60);
        let conditions = WakeConditions {
            wake_secs: self.config.chief.wake_secs,
            status: Some(chief.status),
            work_open: chief_work_open(&self.plans, &self.sessions),
            approval_pending: chief.approval_hold
                || !chief.approvals.is_empty()
                || atomics_core::approvals::menu_visible(&screen),
            input_age: terminal.last_typed().map(|at| at.elapsed()),
            input_empty: chief_input_empty(&screen),
            elapsed: chief.activity.wake_since.map(|at| at.elapsed()),
        };
        if !conditions.should_wake() {
            return;
        }
        let events =
            self.events.iter().filter(|event| event.seq > self.chief_wake_cursor && event.session != chief.id).count();
        let findings = self.heartbeat.last.as_ref().map_or(&[][..], |(_, findings)| findings.as_slice());
        let message = wake_message(events, findings);
        if matches!(self.deliver(index, &message), Delivery::Sent | Delivery::Pending) {
            self.sessions[index].activity.wake_since = Some(Instant::now());
            self.chief_wake_cursor = self.event_seq;
        }
    }

    /// Starts the chief agent in `[chief] cwd`, else in the main checkout of the project on screen,
    /// continuing the `[chief] resume` conversation when one is named. The workspace file's values
    /// win over the main and host config's.
    pub(super) fn start_chief(&mut self, ctx: &egui::Context, prompt: Option<String>) -> Result<String, String> {
        let cwd = self.chief_cwd().unwrap_or_else(|| {
            self.view.clone().or_else(|| self.repos.first().cloned()).unwrap_or_else(|| self.launch_dir.clone())
        });
        let launch = self.tickets.workspace_chief.clone().unwrap_or_default();
        let resume = setting(launch.resume).or_else(|| setting(self.config.chief.resume.clone()));
        let args = self.config.chief.args.clone();
        self.start_as(ctx, "chief", cwd, resume, args, prompt, Role::Chief)
    }

    /// `[chief] cwd`, the workspace file's over the main and host config's. A restart resumes the
    /// chief there too: the folder its hooks last reported follows wherever its shell went.
    pub(super) fn chief_cwd(&self) -> Option<PathBuf> {
        let launch = self.tickets.workspace_chief.as_ref().and_then(|chief| chief.cwd.clone());
        setting(launch).or_else(|| setting(self.config.chief.cwd.clone())).map(|dir| paths::expand(&dir))
    }

    /// `[chief] autostart`, the workspace file's over the main and host config's.
    pub(super) fn chief_autostarts(&self) -> bool {
        self.tickets.workspace_chief.as_ref().and_then(|chief| chief.autostart).unwrap_or(self.config.chief.autostart)
    }

    pub(super) fn chief_dock(&self) -> ChiefDock {
        self.store.chief_dock.unwrap_or(self.config.chief.dock)
    }

    pub(super) fn save_chief_preferences(&mut self) {
        if let Err(err) = self.store.save(&paths::store_file()) {
            self.toast(format!("Could not save chief preferences: {err:#}"));
        }
    }

    pub(super) fn set_chief_dock(&mut self, dock: ChiefDock) {
        self.store.chief_dock = Some(dock);
        self.save_chief_preferences();
        if self.chief_open {
            self.focus_terminal = self.sessions.iter().find(|s| s.chief).map(|s| s.id.clone());
        }
    }

    pub(super) fn toggle_chief(&mut self, ctx: &egui::Context) {
        if !self.sessions.iter().any(|s| s.chief && s.terminal.as_ref().is_some_and(|t| !t.exited())) {
            self.sessions.retain(|s| !s.chief);
            if let Err(err) = self.start_chief(ctx, None) {
                self.toast(err);
                return;
            }
            self.chief_open = true;
        } else {
            self.chief_open = !self.chief_open;
        }
        if self.chief_open
            && let Some(chief) = self.sessions.iter().find(|s| s.chief)
        {
            self.focus_terminal = Some(chief.id.clone());
        }
    }
}

pub(super) fn chief_divider(ui: &mut egui::Ui, rect: Rect, width: &mut Option<f32>, p: &Palette) -> bool {
    let handle = Rect::from_center_size(rect.right_center(), vec2(8.0, rect.height()));
    let response = ui.interact(handle, egui::Id::NULL.with("chief-divider"), Sense::click_and_drag());
    ui.painter().line_segment([rect.right_top(), rect.right_bottom()], Stroke::new(1.0, p.border));
    if response.hovered() || response.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        ui.painter().rect_filled(handle.shrink2(vec2(3.0, 0.0)), 0.0, p.accent.gamma_multiply(0.6));
    }
    if (response.dragged() || response.drag_stopped())
        && let Some(pointer) = response.interact_pointer_pos()
    {
        *width = Some((pointer.x - rect.left()).max(layout::MIN_DOCK_WIDTH));
    }
    if response.double_clicked() {
        *width = None;
    }
    response.drag_stopped() || response.double_clicked()
}

/// The chief's mark: a planet with a tilted orbit and a moon, after the atomisk.ai icon.
fn orbit_icon(painter: &egui::Painter, rect: Rect, accent: Color32, light: Color32) {
    let c = rect.center();
    let r = rect.width() / 2.0;
    painter.circle_filled(c, r * 0.32, light);
    let tilt = -23f32.to_radians();
    let points: Vec<Pos2> = (0..=48)
        .map(|k| {
            let t = k as f32 / 48.0 * std::f32::consts::TAU;
            let (x, y) = (r * 0.95 * t.cos(), r * 0.38 * t.sin());
            pos2(c.x + x * tilt.cos() - y * tilt.sin(), c.y + x * tilt.sin() + y * tilt.cos())
        })
        .collect();
    painter.add(egui::Shape::line(points, Stroke::new(1.4, accent)));
    let moon = pos2(c.x + r * 0.95 * tilt.cos(), c.y + r * 0.95 * tilt.sin());
    painter.circle_filled(moon, r * 0.14, accent);
}

/// Tools the chief may use without asking: reading, planning, reviewing, and running the
/// plan the user approved. Starting agents, typing into terminals and closing sessions ask
/// first. Config tools only read or propose: a proposed change waits for the user to apply it.
/// Checks with atomics_exec run in an agent's sandbox, which reaches no more than its agent does.
pub(super) const CHIEF_ALLOWED_TOOLS: &[&str] = &[
    "mcp__atomics__atomics_list",
    "mcp__atomics__atomics_wait",
    "mcp__atomics__atomics_restart",
    "mcp__atomics__atomics_approve",
    "mcp__atomics__atomics_result",
    "mcp__atomics__atomics_read",
    "mcp__atomics__atomics_diff",
    "mcp__atomics__atomics_exec",
    "mcp__atomics__atomics_plan_get",
    "mcp__atomics__atomics_plan_close",
    "mcp__atomics__atomics_plan_set",
    "mcp__atomics__atomics_plan_update",
    "mcp__atomics__atomics_plan_dispatch",
    "mcp__atomics__atomics_plan_integrate",
    "mcp__atomics__atomics_plan_stage",
    "mcp__atomics__atomics_stage_event",
    "mcp__atomics__atomics_comment",
    "mcp__atomics__atomics_send_review",
    "mcp__atomics__atomics_focus",
    "mcp__atomics__atomics_open",
    "mcp__atomics__atomics_ticket_link",
    "mcp__atomics__atomics_ticket_sync",
    "mcp__atomics__atomics_ticket_comment",
    "mcp__atomics__atomics_config_get",
    "mcp__atomics__atomics_config_propose",
    "mcp__atomics__atomics_doctor",
];

impl Desktop<'_> {
    pub(super) fn chief_pill(&mut self, ui: &mut egui::Ui, compact: bool) {
        let ctx = ui.ctx().clone();
        let p = self.runtime.palette.clone();
        let chief = self.runtime.sessions.iter().position(|s| s.chief);
        let status = chief.map(|i| self.runtime.sessions[i].status);
        let mut toggle = false;
        let mut dock = self.runtime.chief_dock();
        let stroke = if self.runtime.chief_open { p.accent } else { lerp(p.border, p.accent, 0.5) };
        let response = egui::Frame::new()
            .fill(lerp(p.raised, p.accent, 0.08))
            .stroke(Stroke::new(1.0, stroke))
            .corner_radius(13)
            .inner_margin(egui::Margin::symmetric(14, 3))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (icon, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                    orbit_icon(ui.painter(), icon, p.accent, p.foreground);
                    ui.label(RichText::new("CHIEF").strong().color(p.accent).size(13.0));
                    match status {
                        Some(status) => {
                            let (dot, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                            let time = ui.input(|i| i.time);
                            ui.painter().circle_filled(dot.center(), 3.5, status_dot(status, &p, time));
                            if !compact {
                                ui.label(RichText::new(status.label()).color(p.muted).size(11.5));
                            }
                        }
                        None if !compact => {
                            ui.label(RichText::new("start").color(p.muted).size(11.5));
                        }
                        None => {}
                    }
                    if !compact && !self.runtime.plans[self.runtime.selected_plan].tasks.is_empty() {
                        let done = self.runtime.plans[self.runtime.selected_plan].count(TaskStatus::Done);
                        ui.label(
                            RichText::new(format!(
                                "plan {done}/{}",
                                self.runtime.plans[self.runtime.selected_plan].tasks.len()
                            ))
                            .color(p.muted)
                            .size(11.5),
                        );
                    }
                    if !compact && let Some((at, findings)) = &self.runtime.heartbeat.last {
                        if !findings.is_empty() {
                            let (dot, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                            ui.painter().circle_filled(dot.center(), 3.0, p.error);
                        }
                        let age = atomics_core::store::now().saturating_sub(*at);
                        let age_text = match age {
                            0..60 => "now".into(),
                            60..3600 => format!("{}m", age / 60),
                            3600..86400 => format!("{}h", age / 3600),
                            _ => format!("{}d", age / 86400),
                        };
                        ui.ctx().request_repaint_after(Duration::from_secs(60 - age % 60));
                        ui.label(RichText::new(format!("♥ {age_text}")).color(p.muted).size(11.5));
                    }
                    match &self.client.voice {
                        Voice::Recording(recorder) => {
                            let secs = recorder.elapsed().as_secs();
                            let text = format!("● REC {}:{:02}", secs / 60, secs % 60);
                            ui.label(RichText::new(text).color(p.error).strong().size(11.5));
                        }
                        Voice::Opening(_) | Voice::Transcribing => {
                            ui.add(egui::Spinner::new().size(12.0).color(p.accent));
                        }
                        Voice::Idle => {}
                    }
                });
            })
            .response
            .interact(Sense::click());
        super::topbar::record_item(ui, "chief", &response);
        let keys = &self.runtime.config.keys;
        let hover = match self.client.voice {
            Voice::Opening(_) => "Opening microphone: Esc cancels".to_string(),
            Voice::Recording(_) => format!("Recording: {} sends, Esc cancels", shortcut_label(ui.ctx(), &keys.voice)),
            Voice::Transcribing => "Transcribing".to_string(),
            Voice::Idle => format!(
                "The chief agent ({}) · voice ({})",
                shortcut_label(ui.ctx(), &keys.toggle_chief),
                shortcut_label(ui.ctx(), &keys.voice)
            ),
        };
        response.context_menu(|ui| {
            if ui.selectable_value(&mut dock, ChiefDock::Floating, "Float chief").clicked()
                || ui.selectable_value(&mut dock, ChiefDock::Docked, "Dock chief").clicked()
            {
                ui.close();
            }
        });
        if response
            .on_hover_ui(|ui| {
                ui.label(hover);
                if let Some(status) = status {
                    ui.label(status.label());
                }
                if let Some((at, findings)) = &self.runtime.heartbeat.last {
                    let timestamp = *at as libc::time_t;
                    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
                    // SAFETY: both pointers are valid; a non-null result points to initialized local.
                    let local = unsafe { libc::localtime_r(&timestamp, local.as_mut_ptr()).as_ref() };
                    let time = local.map_or_else(
                        || "unavailable".into(),
                        |local| {
                            format!(
                                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                                local.tm_year + 1900,
                                local.tm_mon + 1,
                                local.tm_mday,
                                local.tm_hour,
                                local.tm_min,
                                local.tm_sec
                            )
                        },
                    );
                    ui.label(format!("Last heartbeat: {time} (local)\n{} findings", findings.len()));
                }
            })
            .clicked()
        {
            toggle = true;
        }
        if dock != self.runtime.chief_dock() {
            self.set_chief_dock(dock);
        }
        if toggle {
            self.toggle_chief(&ctx);
        }
    }

    pub(super) fn chief_ui(&mut self, ctx: &egui::Context) {
        let p = self.runtime.palette.clone();
        if !self.runtime.chief_open || self.runtime.last_layout.is_some_and(|layout| layout.dock_width > 0.0) {
            return;
        }
        let screen = ctx.content_rect();
        egui::Window::new("CHIEF AGENT")
            .id(egui::Id::NULL.with("chief-window"))
            .title_bar(false)
            .resizable(true)
            .default_size(vec2(screen.width() * 0.5, screen.height() * 0.66))
            .default_pos(screen.center())
            .pivot(egui::Align2::CENTER_CENTER)
            .frame(
                egui::Frame::window(&ctx.global_style())
                    .fill(p.terminal)
                    .stroke(Stroke::new(1.5, p.accent.gamma_multiply(0.8))),
            )
            .show(ctx, |ui| self.chief_content(ui));
    }

    pub(super) fn chief_content(&mut self, ui: &mut egui::Ui) {
        let Some(index) = self.runtime.sessions.iter().position(|s| s.chief) else { return };
        let p = self.runtime.palette.clone();
        ui.horizontal(|ui| {
            ui.label(RichText::new("◆ CHIEF AGENT").color(p.accent).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("minimize")
                    .on_hover_text(shortcut_label(ui.ctx(), &self.runtime.config.keys.toggle_chief))
                    .clicked()
                {
                    self.runtime.chief_open = false;
                }
                let dock = self.runtime.chief_dock();
                let label = if dock == ChiefDock::Floating { "dock" } else { "float" };
                if ui.small_button(label).clicked() {
                    self.set_chief_dock(if dock == ChiefDock::Floating {
                        ChiefDock::Docked
                    } else {
                        ChiefDock::Floating
                    });
                }
            });
        });
        ui.separator();
        let metrics = self.metrics(ui);
        let rect = ui.available_rect_before_wrap();
        ui.allocate_rect(rect, Sense::hover());
        let session = &mut self.runtime.sessions[index];
        let focus = self.client.focus_terminal.as_deref() == Some(session.id.as_str());
        if let Some(terminal) = &mut session.terminal {
            let id = egui::Id::NULL.with(("chief-term", &session.id));
            if focus {
                ui.memory_mut(|m| m.request_focus(id));
                self.client.focus_terminal = None;
            }
            self.client.terminal_views.entry(id).or_default().show(terminal, ui, rect, id, &metrics, &p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eligible() -> WakeConditions {
        WakeConditions {
            wake_secs: 180,
            status: Some(AgentStatus::Idle),
            work_open: true,
            approval_pending: false,
            input_age: None,
            input_empty: true,
            elapsed: Some(Duration::from_secs(180)),
        }
    }

    #[test]
    fn wakes_idle_or_turn_done_even_without_new_events_or_findings() {
        for status in [AgentStatus::Idle, AgentStatus::Done] {
            assert!(WakeConditions { status: Some(status), ..eligible() }.should_wake());
        }
    }

    #[test]
    fn wake_disabled() {
        assert!(!WakeConditions { wake_secs: 0, ..eligible() }.should_wake());
    }

    #[test]
    fn wake_requires_chief_session() {
        assert!(!WakeConditions { status: None, ..eligible() }.should_wake());
    }

    #[test]
    fn wake_never_interrupts_working_chief() {
        assert!(!WakeConditions { status: Some(AgentStatus::Working), ..eligible() }.should_wake());
    }

    #[test]
    fn wake_suppressed_while_starting_exited_or_needing_input() {
        for status in [AgentStatus::Starting, AgentStatus::Exited, AgentStatus::NeedsInput] {
            assert!(!WakeConditions { status: Some(status), ..eligible() }.should_wake());
        }
    }

    #[test]
    fn wake_requires_open_work() {
        assert!(!WakeConditions { work_open: false, ..eligible() }.should_wake());
    }

    #[test]
    fn wake_suppressed_by_pending_permission_or_approval() {
        assert!(!WakeConditions { approval_pending: true, ..eligible() }.should_wake());
    }

    #[test]
    fn wake_suppressed_for_sixty_seconds_after_typing() {
        assert!(!WakeConditions { input_age: Some(Duration::from_millis(59_999)), ..eligible() }.should_wake());
        assert!(WakeConditions { input_age: Some(Duration::from_secs(60)), ..eligible() }.should_wake());
    }

    #[test]
    fn wake_suppressed_by_unsubmitted_input() {
        assert!(!WakeConditions { input_empty: false, ..eligible() }.should_wake());
    }

    #[test]
    fn wake_waits_full_interval_after_turn_end_or_last_wake() {
        assert!(!WakeConditions { elapsed: Some(Duration::from_millis(179_999)), ..eligible() }.should_wake());
        assert!(!WakeConditions { elapsed: None, ..eligible() }.should_wake());
        assert!(eligible().should_wake());
    }

    #[test]
    fn wake_uses_visible_prompt_line_including_multiline_drafts() {
        for screen in ["❯", "response\n❯ \n────────\n? for shortcuts", "›  ", ">\n\n"] {
            assert!(chief_input_empty(screen), "{screen:?}");
        }
        for screen in ["", "response only", "❯ unsent draft", "❯\n  second draft line\n────", "❯\n❯ draft"]
        {
            assert!(!chief_input_empty(screen), "{screen:?}");
        }
    }

    #[test]
    fn wake_work_includes_running_review_or_non_chief_agents() {
        use super::super::tests::session;
        assert!(!chief_work_open(&[], &[]));
        for status in
            [TaskStatus::Planned, TaskStatus::Blocked, TaskStatus::Done, TaskStatus::Running, TaskStatus::Review]
        {
            let mut plan =
                Plan { id: "open".into(), tasks: vec![Task { status, ..Task::default() }], ..Plan::default() };
            assert_eq!(
                chief_work_open(std::slice::from_ref(&plan), &[]),
                matches!(status, TaskStatus::Running | TaskStatus::Review)
            );
            plan.status = PlanStatus::Completed;
            assert!(!chief_work_open(&[plan], &[]));
        }
        let mut agent = session("agent", AgentKind::Generic, AgentStatus::Idle, Path::new("/tmp"));
        assert!(chief_work_open(&[], std::slice::from_ref(&agent)));
        agent.chief = true;
        assert!(!chief_work_open(&[], std::slice::from_ref(&agent)));
        agent.chief = false;
        agent.kind = AgentKind::Shell;
        assert!(!chief_work_open(&[], &[agent]));
    }

    #[test]
    fn wake_line_is_exact_and_summarizes_unique_short_kinds() {
        assert_eq!(
            wake_message(0, &[]),
            "ATOMICS wake: 0 new events, 0 heartbeat findings (none). Call atomics_wait and act; if nothing changed, post one short line."
        );
        let findings: Vec<_> = [
            Kind::StuckWorking,
            Kind::AgentGone,
            Kind::AgentGone,
            Kind::Approval,
            Kind::Unreported,
            Kind::HeldMessages,
        ]
        .into_iter()
        .map(|kind| Finding { plan: "plan".into(), task: "task".into(), session: None, kind })
        .collect();
        assert_eq!(
            wake_message(3, &findings),
            "ATOMICS wake: 3 new events, 6 heartbeat findings (agent_gone, unreported, approval, held_messages, stuck_working). Call atomics_wait and act; if nothing changed, post one short line."
        );
    }

    #[test]
    fn chief_approval_access_and_read_only_defaults_are_documented() {
        assert!(CHIEF_ALLOWED_TOOLS.contains(&"mcp__atomics__atomics_approve"));
        let readme = include_str!("../../../../README.md");
        assert!(readme.contains("pre-approved `atomics_approve`"));
        assert!(readme.contains("`allow_always` for the rest"));
        assert!(!readme.contains("or answer other agents'"));
        assert!(readme.contains("outside Codex's sandbox"));
        assert!(CHIEF_ALLOWED_TOOLS.contains(&"mcp__atomics__atomics_exec"));
        assert!(readme.contains("pre-approved `atomics_exec`"));
    }
}

#[cfg(test)]
mod chief_tests {
    use super::*;

    #[test]
    fn divider_drags_and_double_click_resets() {
        let ctx = egui::Context::default();
        let palette = Palette::from_config(&atomics_core::config::ThemeConfig::default());
        let mut width = Some(720.0);
        let mut time = 0.0;
        let mut frame = |events| {
            time += 0.05;
            let mut save = false;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(2000.0, 1000.0))),
                time: Some(time),
                events,
                ..Default::default()
            };
            let output = ctx.run_ui(input, |ui| {
                egui::Panel::top("top").exact_size(34.0).show(ui, |_| {});
                egui::Panel::left("sidebar").exact_size(250.0).resizable(false).show(ui, |_| {});
                let rect = egui::Panel::left("chief-dock")
                    .exact_size(width.unwrap_or(595.0))
                    .resizable(false)
                    .frame(egui::Frame::new().inner_margin(egui::Margin::same(6)))
                    .show(ui, |ui| {
                        ui.interact(ui.max_rect(), egui::Id::NULL.with("chief-term"), Sense::click_and_drag());
                        ui.take_available_space();
                    })
                    .response
                    .rect;
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.interact(ui.max_rect(), egui::Id::NULL.with("agent-term"), Sense::click_and_drag());
                });
                save = chief_divider(ui, rect, &mut width, &palette);
            });
            output.drop_without_applying_deltas();
            (width, save)
        };
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let start = pos2(970.0, 400.0);
        let end = pos2(1100.0, 400.0);
        frame(vec![]);
        frame(vec![egui::Event::PointerMoved(start), button(start, true)]);
        assert_eq!(frame(vec![egui::Event::PointerMoved(end)]).0, Some(850.0));
        assert!(frame(vec![button(end, false)]).1);
        frame(vec![]);
        frame(vec![button(end, true)]);
        frame(vec![button(end, false)]);
        frame(vec![button(end, true)]);
        assert_eq!(frame(vec![button(end, false)]), (None, true));
    }
}

/// A set config value: trimmed, and unset when empty.
fn setting(value: Option<String>) -> Option<String> {
    value.map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}
