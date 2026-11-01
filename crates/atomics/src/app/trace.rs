//! Host authorization and persistence around the pure trace reducer.
use super::*;
use atomics_core::trace::{self, TraceLog};

/// Raw events stay in memory until the worker has read the current token heads. Neither disk
/// persistence nor rendering can observe an event cleaned with stale credentials.
pub(super) enum TraceAction {
    Load(String),
    Capture { id: String, event: Box<atomics_core::hook::HookEvent>, plan: Option<String>, at: u64 },
    Respond { reference: Option<String>, limit: Option<usize>, reply: Sender<ControlOutcome> },
}

impl State {
    fn queue_trace(&mut self, config: Arc<atomics_core::tickets::TicketConfig>, action: TraceAction) {
        let seq = self.tickets.next_trace_job;
        self.tickets.next_trace_job += 1;
        self.tickets.trace_jobs.insert(seq, (config.clone(), action));
        self.send_trace_job(&config, seq);
    }

    pub(super) fn finish_trace(&mut self, seq: u64, result: Result<crate::tickets::TicketOutput, String>) {
        let Some((config, action)) = self.tickets.trace_jobs.remove(&seq) else { return };
        let cleaner = match result {
            Ok(crate::tickets::TicketOutput::TraceCleaner(cleaner)) => cleaner,
            _ => {
                if let TraceAction::Respond { reply, .. } = action {
                    let _ =
                        reply.send(ControlOutcome::Done(ControlResponse::error("Could not read trace credentials")));
                } else {
                    self.toast("Could not read trace credentials");
                }
                return;
            }
        };
        match action {
            TraceAction::Load(id) => self.load_chief_trace_with(&id, &cleaner),
            TraceAction::Capture { id, event, plan, at } => {
                self.ingest_chief_trace(&id, &event, Some(&config), &cleaner, at, plan.as_deref());
            }
            TraceAction::Respond { reference, limit, reply } => {
                let response = self.trace_response(reference.as_deref(), limit, &cleaner);
                let _ = reply.send(ControlOutcome::Done(response));
            }
        }
    }

    pub(super) fn load_chief_trace(&mut self, id: &str) {
        if !self.sessions.iter().any(|session| session.id == id && session.chief) || self.traces.contains_key(id) {
            return;
        }
        if let Some(config) = self.ticket_config().filter(|config| config.server.uses_oauth()) {
            if self
                .tickets
                .trace_jobs
                .values()
                .any(|(_, action)| matches!(action, TraceAction::Load(pending) if pending == id))
            {
                return;
            }
            self.queue_trace(config, TraceAction::Load(id.into()));
            return;
        }
        self.load_chief_trace_with(id, &self.cleaner());
    }

    fn load_chief_trace_with(&mut self, id: &str, cleaner: &atomics_core::tickets::Cleaner) {
        if !self.sessions.iter().any(|session| session.id == id && session.chief) || self.traces.contains_key(id) {
            return;
        }
        let mut log = TraceLog::default();
        let home = paths::atomics_home();
        for plan in std::iter::once(None)
            .chain(self.plans.iter().filter(|plan| !plan.id.is_empty()).map(|plan| Some(plan.id.as_str())))
        {
            if let Ok(path) = trace::disk::path(&home, plan) {
                match trace::disk::load(&path, cleaner) {
                    Ok(loaded) => log.merge(loaded),
                    Err(_) => self.toast = Some(("Could not reload chief trace".into(), Instant::now())),
                }
            }
        }
        self.traces.insert(id.into(), log);
    }

    pub(super) fn capture_chief_trace(&mut self, index: usize, event: &atomics_core::hook::HookEvent) {
        // Called only after on_hook's session-id, sandbox and stale-event checks.
        let tool = matches!(event.name.as_str(), "PreToolUse" | "PostToolUse")
            && event.tool_name.as_deref().is_some_and(|name| name.starts_with("mcp__"));
        let config = self.ticket_config();
        let turn = config.as_ref().is_some_and(|c| c.remote.enabled)
            && matches!(event.name.as_str(), "UserPromptSubmit" | "Stop" | "Notification" | "PermissionRequest");
        if !self.sessions[index].chief || !(tool || turn) {
            return;
        }
        let id = self.sessions[index].id.clone();
        if event.name == "UserPromptSubmit" {
            self.tickets.remote.chief_plan =
                self.plans.get(self.selected_plan).filter(|p| p.is_open() && !p.id.is_empty()).map(|p| p.id.clone());
        }
        let plan = event
            .tool_input
            .get("plan")
            .and_then(serde_json::Value::as_str)
            .and_then(|id| self.plans.iter().find(|plan| plan.id == id && plan.is_open()))
            .or_else(|| {
                self.tickets
                    .remote
                    .chief_plan
                    .as_ref()
                    .and_then(|id| self.plans.iter().find(|p| &p.id == id && p.is_open()))
            })
            .or_else(|| self.plans.get(self.selected_plan).filter(|plan| plan.is_open() && !plan.id.is_empty()))
            .map(|plan| plan.id.clone());
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64;
        if let Some(config) = config.as_ref().filter(|config| config.server.uses_oauth()) {
            self.queue_trace(config.clone(), TraceAction::Capture { id, event: Box::new(event.clone()), plan, at });
            return;
        }
        self.ingest_chief_trace(&id, event, config.as_deref(), &self.cleaner(), at, plan.as_deref());
    }

    fn ingest_chief_trace(
        &mut self,
        id: &str,
        event: &atomics_core::hook::HookEvent,
        config: Option<&atomics_core::tickets::TicketConfig>,
        cleaner: &atomics_core::tickets::Cleaner,
        at: u64,
        plan: Option<&str>,
    ) {
        if !self.sessions.iter().any(|session| session.id == id && session.chief) {
            return;
        }
        self.load_chief_trace_with(id, cleaner);
        let entry =
            self.traces.entry(id.into()).or_default().ingest_for_plan(event, config, cleaner, at, plan).cloned();
        if let Some(entry) = &entry {
            let saved = trace::disk::path(&paths::atomics_home(), entry.plan_id.as_deref())
                .and_then(|path| trace::disk::append(&path, entry));
            if saved.is_err() {
                self.toast("Could not save chief trace");
            }
        }
        // A credential job from an old profile must not mirror into a new connection.
        if config == self.ticket_config().as_deref() {
            self.mirror_hook(event, plan, cleaner, entry);
        }
    }

    pub(super) fn remove_plan_trace(&mut self, plan: &str) {
        self.tickets.trace_jobs.retain(
            |_, (_, action)| !matches!(action, TraceAction::Capture { plan: Some(pending), .. } if pending == plan),
        );
        for log in self.traces.values_mut() {
            log.remove_plan(plan);
        }
        if trace::disk::remove(&paths::atomics_home(), plan).is_err() {
            self.toast("Could not remove plan trace");
        }
    }

    pub(super) fn request_trace(
        &mut self,
        reference: Option<String>,
        limit: Option<usize>,
        reply: Sender<ControlOutcome>,
    ) {
        if let Some(config) = self.ticket_config().filter(|config| config.server.uses_oauth()) {
            self.queue_trace(config, TraceAction::Respond { reference, limit, reply });
        } else {
            let response = self.trace_response(reference.as_deref(), limit, &self.cleaner());
            let _ = reply.send(ControlOutcome::Done(response));
        }
    }

    fn trace_response(
        &mut self,
        reference: Option<&str>,
        limit: Option<usize>,
        cleaner: &atomics_core::tickets::Cleaner,
    ) -> ControlResponse {
        let index = match reference {
            Some(reference) => self.find(reference),
            None => self.sessions.iter().position(|session| session.chief),
        };
        let Some(index) = index else {
            return if reference.is_some() {
                ControlResponse::error("No matching chief session")
            } else {
                ControlResponse::ok(json!({"entries": []}))
            };
        };
        if !self.sessions[index].chief {
            return ControlResponse::error("Trace is available for chief sessions only");
        }
        let id = self.sessions[index].id.clone();
        self.load_chief_trace_with(&id, cleaner);
        ControlResponse::ok(
            json!({"session": id, "entries": self.traces.get(&id).map(|log| log.recent(limit.unwrap_or(20)))}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{request, session, with_app};

    #[test]
    fn a_refreshed_token_is_redacted_before_the_trace_is_shown_or_saved() {
        with_app("app::trace::tests::a_refreshed_token_is_redacted_before_the_trace_is_shown_or_saved", |app, _| {
            use crate::mcp_client::{FakeOAuthServer, McpClient};
            let fake = FakeOAuthServer::start();
            fake.state.lock().unwrap().expires_in = 30;
            let (dir, oauth) = crate::mcp_client::oauth_test_machine();
            let server = fake.server();
            oauth
                .sign_in_via(&server, Duration::from_secs(10), &|url| {
                    let url = url.to_string();
                    std::thread::spawn(move || crate::mcp_client::oauth_test_browse(&url));
                    Ok(())
                })
                .unwrap();
            app.runtime.config.tickets = toml::from_str("system = \"echo\"").unwrap();
            app.runtime.config.tickets.server = server.clone();
            app.runtime.apply_ticket_config();
            app.runtime.tickets.sign.override_oauth(oauth.clone());
            let root = app.runtime.launch_dir.clone();
            app.runtime.sessions = vec![session("chief", AgentKind::Claude, AgentStatus::Idle, &root)];
            app.runtime.sessions[0].chief = true;
            let capture = |app: &mut Desktop<'_>, token: &str, id: &str| {
                let event = atomics_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name":"PostToolUse", "session_id":"chief", "tool_name":"mcp__example__read",
                    "tool_use_id":id, "tool_input":{"query":token}, "tool_response":{"text":token}
                }))
                .unwrap();
                app.runtime.capture_chief_trace(0, &event);
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(30));
                while !app.runtime.tickets.trace_jobs.is_empty() {
                    app.runtime.ticket_results();
                    assert!(Instant::now() < deadline, "trace worker timed out");
                    std::thread::sleep(Duration::from_millis(5));
                }
            };
            let old = fake.state.lock().unwrap().access.iter().next().unwrap().clone();
            capture(app, &old, "before");
            // Connecting refreshes the nearly expired token through the real OAuth path.
            fake.state.lock().unwrap().expires_in = 3600;
            let client = McpClient::connect_with(&server, Duration::from_secs(5), |_| None, Some(&oauth)).unwrap();
            let new = client.oauth_token().unwrap();
            assert_ne!(old, new);
            capture(app, new, "after");
            let entries = serde_json::to_string(&app.runtime.traces["chief"].entries).unwrap();
            assert!(entries.contains("***") && !entries.contains(new) && !entries.contains(&old), "{entries}");
            let path = trace::disk::path(&paths::atomics_home(), None).unwrap();
            let saved = std::fs::read_to_string(&path).unwrap();
            assert!(!saved.contains(new) && !saved.contains(&old), "{saved}");
            app.runtime.traces.clear();
            let (reply, received) = channel();
            app.runtime.request_trace(None, None, reply);
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(30));
            let response = loop {
                app.runtime.ticket_results();
                if let Ok(ControlOutcome::Done(response)) = received.try_recv() {
                    break response;
                }
                assert!(Instant::now() < deadline, "trace response timed out");
                std::thread::sleep(Duration::from_millis(5));
            };
            assert!(response.ok);
            assert_eq!(response.result["entries"].as_array().unwrap().len(), 2);
            assert!(!response.result.to_string().contains(new));
            // Even a warm cache must not permit capture when its fingerprint cannot be checked.
            std::fs::write(dir.path().join("fail-keychain"), "").unwrap();
            capture(app, new, "unreadable-keychain");
            assert_eq!(app.runtime.traces["chief"].entries.len(), 2);
            assert_eq!(std::fs::read_to_string(path).unwrap(), saved);
        });
    }

    #[test]
    fn trace_captures_only_chief_and_reloads_the_same_cleaned_entries() {
        with_app("app::trace::tests::trace_captures_only_chief_and_reloads_the_same_cleaned_entries", |app, _| {
            let root = app.runtime.launch_dir.clone();
            app.runtime.sessions = vec![
                session("worker", AgentKind::Claude, AgentStatus::Idle, &root),
                session("chief", AgentKind::Claude, AgentStatus::Idle, &root),
            ];
            app.runtime.sessions[1].chief = true;
            let hook = |pane: &str, session_id: &str, name: &str| {
                HookMessage {
                pane: Some(pane.into()), ancestors: Vec::new(), delivered: 0, approval_reply: None,
                event: atomics_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name":name, "session_id":session_id, "tool_name":"mcp__atomics__atomics_ticket_read",
                    "tool_use_id":"toolu_example", "tool_input":{"ticket":"DEMO-1"},
                    "tool_response":{"structuredContent":{"card":{"system":"example","id":"1","key":"DEMO-1","title":"A ticket"}}}
                })).unwrap(),
            }
            };
            app.runtime.on_hook(hook("worker", "worker", "PostToolUse"));
            assert!(app.runtime.traces.is_empty());
            assert!(!paths::atomics_home().join("trace").exists());
            app.runtime.on_hook(hook("chief", "stale-session", "PreToolUse"));
            assert!(app.runtime.traces.is_empty());
            app.runtime.on_hook(hook("chief", "chief", "PreToolUse"));
            app.runtime.on_hook(hook("chief", "chief", "PostToolUse"));
            let response = request(app, json!({"cmd":"trace_get", "limit":20}));
            assert!(response.ok);
            assert_eq!(response.result["entries"].as_array().unwrap().len(), 1);
            assert_eq!(response.result["entries"][0]["cards"][0]["key"], "DEMO-1");
            assert_eq!(response.result["entries"][0]["state"], "ok");
            assert!(!request(app, json!({"cmd":"trace_get", "session":"worker"})).ok);
            app.runtime.traces.clear();
            assert_eq!(request(app, json!({"cmd":"trace_get", "limit":20})).result, response.result);
            assert_eq!(request(app, json!({"cmd":"trace_get", "limit":0})).result["entries"], json!([]));
            app.runtime.plans.push(Plan { id: "trace-plan".into(), ..Default::default() });
            app.runtime.selected_plan = app.runtime.plans.len() - 1;
            app.runtime.on_hook(hook("chief", "chief", "PostToolUse"));
            let path = trace::disk::path(&paths::atomics_home(), Some("trace-plan")).unwrap();
            assert!(path.exists());
            app.runtime.remove_plan_trace("trace-plan");
            assert!(!path.exists());
            assert!(
                app.runtime.traces["chief"].entries.iter().all(|entry| entry.plan_id.as_deref() != Some("trace-plan"))
            );
        });
    }
}
