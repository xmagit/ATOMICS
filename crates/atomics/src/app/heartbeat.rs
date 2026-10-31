use super::*;
use atomics_core::heartbeat::{self, Finding};

pub(super) struct Activity {
    status: AgentStatus,
    pub status_since: u64,
    pub working_since: Option<u64>,
    pub last_event: u64,
    pub held_since: Option<u64>,
    /// The last turn end or wake, measured independently of wall-clock changes.
    pub wake_since: Option<Instant>,
    /// Unlike the wake deadline, this boundary is unchanged by an automatic wake.
    pub turn_ended: Option<Instant>,
}

impl Default for Activity {
    fn default() -> Self {
        let now = atomics_core::store::now();
        Self {
            status: AgentStatus::Starting,
            status_since: now,
            working_since: None,
            last_event: now,
            held_since: None,
            wake_since: None,
            turn_ended: None,
        }
    }
}

impl Activity {
    pub fn observe(&mut self, status: AgentStatus) {
        self.observe_at(status, Instant::now());
    }

    fn observe_at(&mut self, status: AgentStatus, now: Instant) {
        if self.status != status {
            if !matches!(status, AgentStatus::Idle | AgentStatus::Done) {
                self.wake_since = None;
            } else if !matches!(self.status, AgentStatus::Idle | AgentStatus::Done) {
                self.wake_since = Some(now);
                self.turn_ended = Some(now);
            }
            self.status_since = atomics_core::store::now();
            if status == AgentStatus::Working
                && (self.working_since.is_none() || self.status != AgentStatus::NeedsInput)
            {
                self.working_since = Some(self.status_since);
            }
            self.status = status;
            self.last_event = self.status_since;
        }
    }
}

pub(super) struct Heartbeat {
    started: Instant,
    pending: Option<Receiver<Vec<Finding>>>,
    pub last: Option<(u64, Vec<Finding>)>,
    pub gone: Vec<Finding>,
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self { started: Instant::now(), pending: None, last: None, gone: Vec::new() }
    }
}

impl Heartbeat {
    pub(super) fn deadline(&self, interval: Duration) -> Instant {
        if self.pending.is_some() { Instant::now() + interval } else { self.started + interval }
    }
}

impl State {
    pub(super) fn heartbeat(&mut self) {
        if let Some(findings) = self.heartbeat.pending.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.heartbeat.pending = None;
            let at = atomics_core::store::now();
            self.event_seq += 1;
            self.events.push(StatusEvent {
                seq: self.event_seq,
                session: String::new(),
                title: "Fleet heartbeat".into(),
                status: AgentStatus::Idle,
                at,
                plan: None,
                task: None,
                report: None,
                approval: None,
                findings: Some(findings.clone()),
                config: None,
                ticket: None,
                change: None,
            });
            self.heartbeat.last = Some((at, findings));
            self.serve_waiters();
        }
        let interval = Duration::from_secs(self.config.chief.heartbeat_secs.max(1));
        let remaining = interval.saturating_sub(self.heartbeat.started.elapsed());
        if self.heartbeat.pending.is_some() {
            return;
        }
        if !remaining.is_zero() {
            return;
        }
        self.heartbeat.started = Instant::now();
        self.ticket_heartbeat();
        let plans: Vec<_> = self.plans.iter().filter(|p| p.is_open()).cloned().collect();
        let sessions: Vec<_> = self
            .sessions
            .iter()
            .filter(|s| plans.iter().any(|p| p.contains_session(&s.id)))
            .map(|s| {
                (
                    heartbeat::Session {
                        id: s.id.clone(),
                        status: if s.terminal.as_ref().is_some_and(Terminal::exited) {
                            AgentStatus::Exited
                        } else {
                            s.status
                        },
                        status_since: s.activity.status_since,
                        working_since: s.activity.working_since,
                        last_event: s.activity.last_event,
                        approval: s.sandbox.is_none() && !s.approvals.is_empty(),
                        held_since: s.activity.held_since,
                        screen: String::new(),
                    },
                    s.terminal.as_ref().filter(|_| s.sandbox.is_none()).map(|t| t.screen_reader(3)),
                )
            })
            .collect();
        let mut gone = std::mem::take(&mut self.heartbeat.gone);
        gone.retain(|f| {
            plans
                .iter()
                .any(|p| p.id == f.plan && p.tasks.iter().any(|t| t.id == f.task && t.status == TaskStatus::Blocked))
        });
        let (tx, rx) = self.wake.channel();
        self.heartbeat.pending = Some(rx);
        std::thread::spawn(move || {
            let sessions: Vec<_> = sessions
                .into_iter()
                .map(|(mut s, read)| {
                    if let Some(read) = read {
                        s.screen = read();
                    }
                    s
                })
                .collect();
            let mut findings = heartbeat::check(&plans, &sessions, atomics_core::store::now());
            for finding in gone {
                if !findings.contains(&finding) {
                    findings.push(finding);
                }
            }
            let _ = tx.send(findings);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{session, with_app};
    use super::*;

    fn finish(app: &mut Desktop<'_>) {
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(3));
        while app.runtime.heartbeat.pending.is_some() {
            assert!(Instant::now() < deadline, "heartbeat worker did not finish");
            std::thread::sleep(Duration::from_millis(5));
            app.runtime.heartbeat();
        }
    }

    #[test]
    fn working_turn_start_survives_tools_approvals_and_idle_until_next_turn() {
        let mut activity = Activity::default();
        assert!(activity.working_since.is_none());
        activity.observe(AgentStatus::Working);
        assert_eq!(activity.working_since, Some(activity.status_since));
        activity.working_since = Some(1);
        for status in
            [AgentStatus::Working, AgentStatus::NeedsInput, AgentStatus::Working, AgentStatus::Done, AgentStatus::Idle]
        {
            activity.observe(status);
            assert_eq!(activity.working_since, Some(1));
        }
        activity.observe(AgentStatus::Working);
        assert!(activity.working_since.unwrap() > 1);
        assert_eq!(activity.working_since, Some(activity.status_since));
    }

    #[test]
    fn chief_wake_timer_resets_on_turn_start_and_restarts_at_turn_end() {
        let now = Instant::now();
        let mut activity = Activity::default();
        activity.observe_at(AgentStatus::Idle, now);
        assert_eq!(activity.wake_since, Some(now));
        activity.wake_since = Some(now + Duration::from_secs(180)); // A wake was sent.
        activity.observe_at(AgentStatus::Idle, now + Duration::from_secs(181));
        assert_eq!(activity.wake_since, Some(now + Duration::from_secs(180)));
        assert_eq!(activity.turn_ended, Some(now), "a wake must not clear native input history");
        activity.observe_at(AgentStatus::Working, now + Duration::from_secs(182));
        assert_eq!(activity.wake_since, None);
        activity.observe_at(AgentStatus::NeedsInput, now + Duration::from_secs(190));
        activity.observe_at(AgentStatus::Working, now + Duration::from_secs(200));
        assert_eq!(activity.wake_since, None);
        activity.observe_at(AgentStatus::Done, now + Duration::from_secs(500));
        assert_eq!(activity.wake_since, Some(now + Duration::from_secs(500)));
        assert_eq!(activity.turn_ended, Some(now + Duration::from_secs(500)));
        activity.observe_at(AgentStatus::Idle, now + Duration::from_secs(510));
        assert_eq!(activity.wake_since, Some(now + Duration::from_secs(500)));
        // A complete turn between runtime ticks still resets the timer through hook observations.
        activity.observe_at(AgentStatus::Working, now + Duration::from_secs(520));
        activity.observe_at(AgentStatus::Done, now + Duration::from_secs(521));
        assert_eq!(activity.wake_since, Some(now + Duration::from_secs(521)));
    }

    #[test]
    fn interval_emits_one_empty_event_to_waiters_and_updates_pill_state() {
        with_app(
            "app::heartbeat::tests::interval_emits_one_empty_event_to_waiters_and_updates_pill_state",
            |app, ctx| {
                app.runtime.config.chief.heartbeat_secs = 7;
                app.runtime.heartbeat();
                assert!(app.runtime.heartbeat.pending.is_none());
                assert!(app.runtime.heartbeat.last.is_none());
                let (tx, rx) = channel();
                app.runtime.control(
                    ctx,
                    ControlRequest::Wait {
                        sessions: vec![],
                        statuses: vec!["heartbeat".into()],
                        after: Some(0),
                        timeout_secs: Some(30),
                    },
                    tx,
                );
                app.runtime.heartbeat.started = Instant::now() - Duration::from_secs(7);
                app.runtime.heartbeat();
                assert!(app.runtime.heartbeat.pending.is_some());
                finish(app);
                let ControlOutcome::Done(response) =
                    rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(1))).unwrap()
                else {
                    panic!("wait did not finish")
                };
                assert!(response.ok);
                assert_eq!(response.result["events"].as_array().unwrap().len(), 1);
                assert_eq!(response.result["events"][0]["status"], "heartbeat");
                assert_eq!(response.result["events"][0]["findings"], json!([]));
                assert_eq!(response.result["cursor"], 1);
                assert_eq!(response.result["timed_out"], false);
                assert!(app.runtime.heartbeat.last.as_ref().unwrap().1.is_empty());
                app.runtime.heartbeat();
                assert_eq!(app.runtime.event_seq, 1);
                app.runtime.heartbeat.started -= Duration::from_secs(7);
                app.runtime.heartbeat();
                finish(app);
                assert_eq!(app.runtime.event_seq, 2);
            },
        );
    }

    #[test]
    fn gone_agents_survive_plan_sync_and_filtered_waiters_receive_findings() {
        with_app(
            "app::heartbeat::tests::gone_agents_survive_plan_sync_and_filtered_waiters_receive_findings",
            |app, ctx| {
                app.runtime.sessions.push(session(
                    "gone",
                    AgentKind::Generic,
                    AgentStatus::Exited,
                    &paths::atomics_home(),
                ));
                app.runtime.plans = vec![Plan {
                    id: "open".into(),
                    tasks: vec![Task {
                        id: "task".into(),
                        session: Some("gone".into()),
                        status: TaskStatus::Running,
                        ..Task::default()
                    }],
                    ..Plan::default()
                }];
                let (tx, rx) = channel();
                app.runtime.control(
                    ctx,
                    ControlRequest::Wait {
                        sessions: vec!["gone".into()],
                        statuses: vec!["heartbeat".into()],
                        after: Some(0),
                        timeout_secs: Some(30),
                    },
                    tx,
                );
                app.runtime.sync_plan();
                assert_eq!(app.runtime.plans[0].tasks[0].status, TaskStatus::Blocked);
                app.runtime.heartbeat.started -= Duration::from_secs(300);
                app.runtime.heartbeat();
                finish(app);
                let ControlOutcome::Done(response) =
                    rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(1))).unwrap()
                else {
                    panic!("wait did not finish")
                };
                assert_eq!(
                    response.result["events"][0]["findings"][0],
                    json!({
                        "plan":"open", "task":"task", "session":"gone", "kind":"agent_gone"
                    })
                );
                assert_eq!(app.runtime.heartbeat.last.as_ref().unwrap().1.len(), 1);
            },
        );
    }

    #[test]
    fn activity_tracks_status_hooks_reports_and_oldest_held_message() {
        with_app("app::heartbeat::tests::activity_tracks_status_hooks_reports_and_oldest_held_message", |app, _| {
            app.runtime.sessions.push(session(
                "agent",
                AgentKind::Claude,
                AgentStatus::Working,
                &paths::atomics_home(),
            ));
            app.runtime.record_events();
            app.runtime.sessions[0].activity.last_event = 1;
            app.runtime.sessions[0].activity.status_since = 2;
            app.runtime.record_events();
            assert_eq!(app.runtime.sessions[0].activity.last_event, 1, "frames are not agent events");
            assert_eq!(app.runtime.sessions[0].activity.status_since, 2);
            app.runtime.report(Some("agent"), "progress", "working");
            assert!(app.runtime.sessions[0].activity.last_event > 1);
            app.runtime.sessions[0].activity.last_event = 1;
            app.runtime.on_hook(HookMessage {
                pane: Some("agent".into()),
                ancestors: vec![],
                delivered: 0,
                approval_reply: None,
                event: atomics_core::hook::HookEvent {
                    name: "PostToolUse".into(),
                    session_id: "agent".into(),
                    ..Default::default()
                },
            });
            assert!(app.runtime.sessions[0].activity.last_event > 1, "same-status hooks refresh activity");
            app.runtime.sessions[0].status = AgentStatus::Done;
            app.runtime.record_events();
            assert!(app.runtime.sessions[0].activity.status_since > 2);
            app.runtime.sessions[0].approval_hold = true;
            assert_eq!(app.runtime.deliver(0, "first"), Delivery::Held);
            app.runtime.sessions[0].activity.held_since = Some(3);
            assert_eq!(app.runtime.deliver(0, "second"), Delivery::Held);
            assert_eq!(app.runtime.sessions[0].activity.held_since, Some(3));
            app.runtime.sessions[0].approval_hold = false;
            app.runtime.release_held(0);
            assert!(app.runtime.sessions[0].activity.held_since.is_none());
        });
    }

    #[test]
    fn chief_pill_renders_last_heartbeat_and_finding_dot() {
        with_app("app::heartbeat::tests::chief_pill_renders_last_heartbeat_and_finding_dot", |app, ctx| {
            ctx.global_style_mut(|style| {
                style.animation_time = 0.0;
                style.interaction.tooltip_delay = 0.0;
            });
            let finding =
                Finding { plan: "open".into(), task: "task".into(), session: None, kind: heartbeat::Kind::AgentGone };
            let mut time = 0.0;
            for (age, label, findings) in
                [(0, "♥ now", vec![finding]), (120, "♥ 2m", vec![]), (7200, "♥ 2h", vec![]), (172_800, "♥ 2d", vec![])]
            {
                let has_findings = !findings.is_empty();
                let at = atomics_core::store::now() - age;
                app.runtime.heartbeat.last = Some((at, findings));
                let error = app.runtime.palette.error;
                let mut frame = |events| {
                    time += 1.0;
                    ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 800.0))),
                            time: Some(time),
                            events,
                            ..Default::default()
                        },
                        |ui| app.top_bar(ui),
                    )
                };
                frame(vec![]).drop_without_applying_deltas();
                let output = frame(vec![]);
                let position = output
                    .shapes
                    .iter()
                    .find_map(|shape| match &shape.shape {
                        egui::Shape::Text(text) if text.galley.text() == label => {
                            Some(text.pos + text.galley.rect.center().to_vec2())
                        }
                        _ => None,
                    })
                    .expect("compact heartbeat age");
                assert_eq!(
                    output.shapes.iter().any(|shape| {
                        matches!(&shape.shape, egui::Shape::Circle(circle)
                            if circle.radius == 3.0 && circle.fill == error)
                    }),
                    has_findings
                );
                output.drop_without_applying_deltas();
                frame(vec![egui::Event::PointerMoved(position)]).drop_without_applying_deltas();
                let output = frame(vec![]);
                let mut date = std::process::Command::new("date");
                if cfg!(target_os = "macos") {
                    date.args(["-r", &at.to_string()]);
                } else {
                    date.args(["-d", &format!("@{at}")]);
                }
                let local = crate::test_support::output(date.arg("+%Y-%m-%d %H:%M:%S"));
                assert!(local.status.success());
                let expected = format!("Last heartbeat: {} (local)", String::from_utf8(local.stdout).unwrap().trim());
                assert!(
                    output.shapes.iter().any(|shape| {
                        matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text().contains(&expected))
                    }),
                    "hover must show {expected}: {:?}",
                    output
                        .shapes
                        .iter()
                        .filter_map(|shape| match &shape.shape {
                            egui::Shape::Text(text) => Some(text.galley.text()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                );
                output.drop_without_applying_deltas();
            }
        });
    }
}
