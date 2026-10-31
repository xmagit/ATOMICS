//! Host authorization and persistence around the pure trace reducer.
use super::*;
use atomics_core::trace::{self, TraceLog};

impl State {
    fn trace_cleaner(&self) -> atomics_core::tickets::Cleaner {
        let cleaner = self.cleaner();
        match self.ticket_config() {
            Some(config) => cleaner.with(crate::mcp_client::OAuth::current().secrets(&config.server)),
            None => cleaner,
        }
    }

    pub(super) fn load_chief_trace(&mut self, id: &str) {
        if !self.sessions.iter().any(|session| session.id == id && session.chief) || self.traces.contains_key(id) {
            return;
        }
        let cleaner = self.trace_cleaner();
        let mut log = TraceLog::default();
        let home = paths::atomics_home();
        for plan in std::iter::once(None)
            .chain(self.plans.iter().filter(|plan| !plan.id.is_empty()).map(|plan| Some(plan.id.as_str())))
        {
            if let Ok(path) = trace::disk::path(&home, plan) {
                match trace::disk::load(&path, &cleaner) {
                    Ok(loaded) => log.merge(loaded),
                    Err(_) => self.toast = Some(("Could not reload chief trace".into(), Instant::now())),
                }
            }
        }
        self.traces.insert(id.into(), log);
    }

    pub(super) fn capture_chief_trace(&mut self, index: usize, event: &atomics_core::hook::HookEvent) {
        // Called only after on_hook's session-id, sandbox and stale-event checks.
        if !self.sessions[index].chief
            || !matches!(event.name.as_str(), "PreToolUse" | "PostToolUse")
            || !event.tool_name.as_deref().is_some_and(|name| name.starts_with("mcp__"))
        {
            return;
        }
        let id = self.sessions[index].id.clone();
        self.load_chief_trace(&id);
        let config = self.ticket_config();
        let cleaner = self.trace_cleaner();
        let plan = event
            .tool_input
            .get("plan")
            .and_then(serde_json::Value::as_str)
            .and_then(|id| self.plans.iter().find(|plan| plan.id == id && plan.is_open()))
            .or_else(|| self.plans.get(self.selected_plan).filter(|plan| plan.is_open() && !plan.id.is_empty()))
            .map(|plan| plan.id.clone());
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64;
        let Some(entry) =
            self.traces.entry(id).or_default().ingest_for_plan(event, config.as_deref(), &cleaner, at, plan.as_deref())
        else {
            return;
        };
        let saved = trace::disk::path(&paths::atomics_home(), entry.plan_id.as_deref())
            .and_then(|path| trace::disk::append(&path, entry));
        if saved.is_err() {
            self.toast("Could not save chief trace");
        }
    }

    pub(super) fn remove_plan_trace(&mut self, plan: &str) {
        for log in self.traces.values_mut() {
            log.remove_plan(plan);
        }
        if trace::disk::remove(&paths::atomics_home(), plan).is_err() {
            self.toast("Could not remove plan trace");
        }
    }

    pub(super) fn trace_response(&mut self, reference: Option<&str>, limit: Option<usize>) -> ControlResponse {
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
        self.load_chief_trace(&id);
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
            let response = request(app, json!({"cmd":"trace", "limit":20}));
            assert!(response.ok);
            assert_eq!(response.result["entries"].as_array().unwrap().len(), 1);
            assert_eq!(response.result["entries"][0]["cards"][0]["key"], "DEMO-1");
            assert_eq!(response.result["entries"][0]["state"], "ok");
            assert!(!request(app, json!({"cmd":"trace", "session":"worker"})).ok);
            app.runtime.traces.clear();
            assert_eq!(request(app, json!({"cmd":"trace", "limit":20})).result, response.result);
            assert_eq!(request(app, json!({"cmd":"trace", "limit":0})).result["entries"], json!([]));
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
