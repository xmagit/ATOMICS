use super::*;
use imacs_core::heartbeat::{self, Finding};

pub(super) struct Activity {
    status: AgentStatus,
    pub status_since: u64,
    pub working_since: Option<u64>,
    pub last_event: u64,
    pub held_since: Option<u64>,
}

impl Default for Activity {
    fn default() -> Self {
        let now = imacs_core::store::now();
        Self {
            status: AgentStatus::Starting,
            status_since: now,
            working_since: None,
            last_event: now,
            held_since: None,
        }
    }
}

impl Activity {
    pub fn observe(&mut self, status: AgentStatus) {
        if self.status != status {
            self.status_since = imacs_core::store::now();
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
            let at = imacs_core::store::now();
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
                        approval: !s.approvals.is_empty(),
                        held_since: s.activity.held_since,
                        screen: String::new(),
                    },
                    s.terminal.as_ref().map(|t| t.screen_reader(3)),
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
        let ctx = self.ctx.clone();
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
            let mut findings = heartbeat::check(&plans, &sessions, imacs_core::store::now());
            for finding in gone {
                if !findings.contains(&finding) {
                    findings.push(finding);
                }
            }
            let _ = tx.send(findings);
            ctx.request_repaint();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{session, with_app};
    use super::*;

    fn finish(app: &mut State) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while app.heartbeat.pending.is_some() {
            assert!(Instant::now() < deadline, "heartbeat worker did not finish");
            std::thread::sleep(Duration::from_millis(5));
            app.heartbeat();
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
    fn interval_emits_one_empty_event_to_waiters_and_updates_pill_state() {
        with_app(
            "app::heartbeat::tests::interval_emits_one_empty_event_to_waiters_and_updates_pill_state",
            |app, ctx| {
                app.config.chief.heartbeat_secs = 7;
                app.heartbeat();
                assert!(app.heartbeat.pending.is_none());
                assert!(app.heartbeat.last.is_none());
                let (tx, rx) = channel();
                app.control(
                    ctx,
                    ControlRequest::Wait {
                        sessions: vec![],
                        statuses: vec!["heartbeat".into()],
                        after: Some(0),
                        timeout_secs: Some(30),
                    },
                    tx,
                );
                app.heartbeat.started = Instant::now() - Duration::from_secs(7);
                app.heartbeat();
                assert!(app.heartbeat.pending.is_some());
                finish(app);
                let ControlOutcome::Done(response) = rx.recv_timeout(Duration::from_secs(1)).unwrap() else {
                    panic!("wait did not finish")
                };
                assert!(response.ok);
                assert_eq!(response.result["events"].as_array().unwrap().len(), 1);
                assert_eq!(response.result["events"][0]["status"], "heartbeat");
                assert_eq!(response.result["events"][0]["findings"], json!([]));
                assert_eq!(response.result["cursor"], 1);
                assert_eq!(response.result["timed_out"], false);
                assert!(app.heartbeat.last.as_ref().unwrap().1.is_empty());
                app.heartbeat();
                assert_eq!(app.event_seq, 1);
                app.heartbeat.started -= Duration::from_secs(7);
                app.heartbeat();
                finish(app);
                assert_eq!(app.event_seq, 2);
            },
        );
    }

    #[test]
    fn gone_agents_survive_plan_sync_and_filtered_waiters_receive_findings() {
        with_app(
            "app::heartbeat::tests::gone_agents_survive_plan_sync_and_filtered_waiters_receive_findings",
            |app, ctx| {
                app.sessions.push(session("gone", AgentKind::Generic, AgentStatus::Exited, &paths::imacs_home()));
                app.plans = vec![Plan {
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
                app.control(
                    ctx,
                    ControlRequest::Wait {
                        sessions: vec!["gone".into()],
                        statuses: vec!["heartbeat".into()],
                        after: Some(0),
                        timeout_secs: Some(30),
                    },
                    tx,
                );
                app.sync_plan();
                assert_eq!(app.plans[0].tasks[0].status, TaskStatus::Blocked);
                app.heartbeat.started -= Duration::from_secs(300);
                app.heartbeat();
                finish(app);
                let ControlOutcome::Done(response) = rx.recv_timeout(Duration::from_secs(1)).unwrap() else {
                    panic!("wait did not finish")
                };
                assert_eq!(
                    response.result["events"][0]["findings"][0],
                    json!({
                        "plan":"open", "task":"task", "session":"gone", "kind":"agent_gone"
                    })
                );
                assert_eq!(app.heartbeat.last.as_ref().unwrap().1.len(), 1);
            },
        );
    }

    #[test]
    fn activity_tracks_status_hooks_reports_and_oldest_held_message() {
        with_app("app::heartbeat::tests::activity_tracks_status_hooks_reports_and_oldest_held_message", |app, _| {
            app.sessions.push(session("agent", AgentKind::Claude, AgentStatus::Working, &paths::imacs_home()));
            app.record_events();
            app.sessions[0].activity.last_event = 1;
            app.sessions[0].activity.status_since = 2;
            app.record_events();
            assert_eq!(app.sessions[0].activity.last_event, 1, "frames are not agent events");
            assert_eq!(app.sessions[0].activity.status_since, 2);
            app.report(Some("agent"), "progress", "working");
            assert!(app.sessions[0].activity.last_event > 1);
            app.sessions[0].activity.last_event = 1;
            app.on_hook(HookMessage {
                pane: Some("agent".into()),
                ancestors: vec![],
                delivered: 0,
                approval_reply: None,
                event: imacs_core::hook::HookEvent {
                    name: "PostToolUse".into(),
                    session_id: "agent".into(),
                    ..Default::default()
                },
            });
            assert!(app.sessions[0].activity.last_event > 1, "same-status hooks refresh activity");
            app.sessions[0].status = AgentStatus::Done;
            app.record_events();
            assert!(app.sessions[0].activity.status_since > 2);
            app.sessions[0].approval_hold = true;
            assert_eq!(app.deliver(0, "first"), Delivery::Held);
            app.sessions[0].activity.held_since = Some(3);
            assert_eq!(app.deliver(0, "second"), Delivery::Held);
            assert_eq!(app.sessions[0].activity.held_since, Some(3));
            app.sessions[0].approval_hold = false;
            app.release_held(0);
            assert!(app.sessions[0].activity.held_since.is_none());
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
                let at = imacs_core::store::now() - age;
                app.heartbeat.last = Some((at, findings));
                let error = app.palette.error;
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
