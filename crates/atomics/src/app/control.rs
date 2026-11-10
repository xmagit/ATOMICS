use atomics_core::control::{MAX_READ_LINES, clip, clip_json, clip_list, clip_start};

use super::*;

/// The characters of screen text `read` and `result` show.
const MAX_SCREEN_CHARS: usize = 40_000;
/// An agent's final message as `result` shows it, and a prompt.
const MAX_MESSAGE_CHARS: usize = 24_000;
const MAX_PROMPT_CHARS: usize = 2000;

impl State {
    pub(super) fn describe(&self, index: usize) -> serde_json::Value {
        let s = &self.sessions[index];
        let stats = self.diffs.get(&s.root()).and_then(|d| d.prepared.as_ref()).map(|p| {
            json!({
                "files": p.diff.files.len(), "additions": p.additions, "deletions": p.deletions
            })
        });
        json!({
            "id": s.id,
            "chief": s.chief,
            "title": s.title,
            "agent": s.agent,
            "status": s.status.label(),
            "folder": s.cwd,
            "worktree": s.worktree.as_ref().map(|w| &w.root),
            "branch": s.branch(),
            "outside_atomics": s.terminal.is_none() && s.status != AgentStatus::Starting,
            // The agent's process on the machine the host runs on (the VM's, with the host there).
            "pid": s.terminal.as_ref().map(|t| t.process_id()),
            "claude_session_id": s.claude_id,
            "sandbox": s.sandbox,
            "scratch": s.scratch,
            "last_prompt": s.last_prompt.as_deref().map(|prompt| clip(prompt, MAX_PROMPT_CHARS)),
            "approval": s.approvals.first().map(|p| {
                let mut request = p.json();
                clip_json(&mut request, MAX_PROMPT_CHARS, 50);
                request
            }),
            // The agent's own menu, answered with its keys, when no hook asked.
            "approval_menu": s.approvals.first().and_then(|p| p.native.as_ref()).map(|menu| menu.kind.label()),
            "queue": s.terminal.as_ref().and_then(|t| {
                let (running, waiting) = self.governor.snapshot();
                queue_note(t.process_id(), &running, &waiting)
            }),
            "diff": stats,
            "previews": self.previews_json(index),
        })
    }

    /// The session's previews, as clients are offered them: what it declared with atomics_preview, then
    /// listeners found in its process tree (Linux). None once it exited. Whether a server speaks TLS is
    /// only found out when a client opens it, so `url` is the plain-HTTP address on this host.
    fn previews_json(&self, index: usize) -> serde_json::Value {
        let s = &self.sessions[index];
        if s.status == AgentStatus::Exited {
            return json!([]);
        }
        let declared = self.previews.declared.get(&s.id);
        let previews: Vec<_> = self
            .previews
            .list(&s.id)
            .into_iter()
            .map(|p| {
                let source =
                    if declared.is_some_and(|d| d.iter().any(|d| d.port == p.port)) { "declared" } else { "detected" };
                json!({
                    "port": p.port,
                    "path": p.path,
                    "label": p.label,
                    "title": p.title(),
                    "url": p.url(p.port),
                    "source": source,
                })
            })
            .collect();
        json!(previews)
    }

    /// The refusal of a decision that names no request, or one that is not the pending one: it says which
    /// request is pending, in the error and as `result.pending`, to be read and answered by its id.
    fn unseen_request(&self, index: usize, named: Option<&str>) -> ControlResponse {
        let pending = self.sessions[index].approvals.first().map(|p| {
            let mut request = p.json();
            clip_json(&mut request, MAX_PROMPT_CHARS, 50);
            request
        });
        let shown = pending.as_ref().map(|p| p.to_string()).unwrap_or_default();
        let id = pending.as_ref().and_then(|p| p["id"].as_str()).unwrap_or_default().to_string();
        let error = match named {
            None => {
                format!("name the request you answer: pass request with its id. The pending request is {id}: {shown}")
            }
            Some(named) => format!(
                "request {named} is no longer pending, so this decision answers nothing. The pending request is now                  {id}: {shown}. Read it, then answer with request {id} if you mean to."
            ),
        };
        ControlResponse { ok: false, result: json!({ "pending": pending }), error: Some(error) }
    }

    pub(super) fn control(&mut self, request: ControlRequest, reply: Sender<ControlOutcome>) {
        if self.restarting {
            let _ = reply.send(ControlOutcome::Done(ControlResponse::error("ATOMICS is restarting")));
            return;
        }
        if !self.window.is_attached()
            && let Some(refusal) = headless_refusal(&request)
        {
            let _ = reply.send(ControlOutcome::Done(ControlResponse::error(refusal)));
            return;
        }
        let done = |response: ControlResponse| ControlOutcome::Done(response);
        let missing = |reference: &str| done(ControlResponse::error(format!("no session matches {reference:?}")));
        let target = match &request {
            ControlRequest::PlanSet { plan: Some(id), .. } | ControlRequest::PlanClose { plan: id, .. } => {
                Some(Some(id.as_str()))
            }
            ControlRequest::PlanUpdate { plan, .. }
            | ControlRequest::PlanApprove { plan }
            | ControlRequest::PlanDispatch { plan, .. }
            | ControlRequest::PlanIntegrate { plan, .. }
            | ControlRequest::TicketLink { plan, .. }
            | ControlRequest::TicketComment { plan, .. } => Some(plan.as_deref()),
            _ => None,
        };
        // plan_set may also name a completed plan, which it reopens once the request is known to be valid.
        let completed = match &request {
            ControlRequest::PlanSet { plan: Some(id), .. } => {
                self.plans.iter().position(|p| p.id == *id && p.status == plan::PlanStatus::Completed)
            }
            _ => None,
        };
        let target = if let Some(index) = completed {
            index
        } else if let Some(id) = target {
            match plan::resolve_plan(&self.plans, id) {
                Ok(index) => index,
                Err(err) => {
                    let _ = reply.send(done(ControlResponse::error(err)));
                    return;
                }
            }
        } else {
            self.selected_plan
        };
        let outcome = match request {
            ControlRequest::AgentTicketGet { pane, plan } => {
                let pane = self.pane_session(pane);
                self.agent_ticket(pane.as_deref(), plan, None, reply);
                return;
            }
            ControlRequest::AgentTicketComment { pane, text } => {
                let pane = self.pane_session(pane);
                self.agent_ticket(pane.as_deref(), false, Some(text), reply);
                return;
            }
            ControlRequest::McpConnect { pane, server, url } => {
                done(self.request_connection(pane, &server, url.as_deref()))
            }
            ControlRequest::OpenLink { plan, ticket, system, rejected } => {
                let incoming = match rejected {
                    Some(reason) => Err(reason),
                    None => crate::open_link::OpenLink { plan, ticket, system }.validate(),
                };
                // A ticket link that arrives before the ticket state is known waits for it, as one
                // from the command line does: answering "not configured" would be a guess.
                if matches!(&incoming, Ok(link) if link.ticket.is_some()) && !self.ticket_state_known() {
                    self.hold_link(incoming);
                    done(ControlResponse::ok(json!({})))
                } else {
                    done(match self.open_link(incoming) {
                        Ok(()) => ControlResponse::ok(json!({})),
                        Err(message) => ControlResponse::error(message),
                    })
                }
            }
            ControlRequest::Restart { force, reason, pane } => done(self.restart(force, reason, pane.as_deref())),
            ControlRequest::UpdateInstalled { version, program } => done(self.update_installed(version, program)),
            ControlRequest::Remote => done(ControlResponse::ok(self.remote_json())),
            ControlRequest::PairClient { name } => done(self.pair_client(&name)),
            ControlRequest::UnpairClient { name } => done(self.unpair_client(&name)),
            ControlRequest::Approve { session, request, decision, message } => match self.find(&session) {
                Some(index) => {
                    // What the screen shows now decides, not what the last poll saw.
                    self.approvable(index);
                    let approval = self.sessions[index].approvals.first().map(|pending| pending.id.clone());
                    let menu = self.sessions[index].approval_hold
                        || self.sessions[index]
                            .terminal
                            .as_ref()
                            .is_some_and(|terminal| atomics_core::approvals::menu_visible(&terminal.screen_text(60)));
                    done(match approval {
                        None if menu => ControlResponse::error(
                            "the menu on screen is not a command approval; read it and answer with atomics_send raw keys",
                        ),
                        None => ControlResponse::error(match request {
                            Some(request) => format!("request {request} is no longer pending, and nothing else is"),
                            None => "no pending permission request".into(),
                        }),
                        // A decision answers only the request it names: one made about an earlier request
                        // must never answer a newer one its maker has not seen.
                        Some(approval) if request.as_deref() != Some(approval.as_str()) => {
                            self.unseen_request(index, request.as_deref())
                        }
                        Some(approval) => match self.approve(index, &approval, decision, message.as_deref()) {
                            Ok(answered) => ControlResponse::ok(answered.json()),
                            Err(error) if error.code == atomics_core::wire::ErrorCode::StaleRevision => {
                                self.unseen_request(index, request.as_deref())
                            }
                            Err(error) => ControlResponse::error(error.message),
                        },
                    })
                }
                None => missing(&session),
            },
            ControlRequest::TraceGet { session, limit } => {
                self.request_trace(session, limit, reply);
                return;
            }
            ControlRequest::List => {
                self.usage.drain();
                // Every session, so each one's prompt and pending request are kept short.
                let sessions: Vec<_> = (0..self.sessions.len())
                    .map(|i| {
                        let mut session = self.describe(i);
                        session["last_prompt"] = json!(self.sessions[i].last_prompt.as_deref().map(|p| clip(p, 300)));
                        session["approval"] = json!(self.sessions[i].approvals.first().map(PendingApproval::json));
                        clip_json(&mut session["approval"], 300, 20);
                        session
                    })
                    .collect();
                // The headless host has no window: both are null there.
                let viewport = self.window.viewport();
                let layout = self.last_layout.map(|l| {
                    json!({ "sidebar": l.sidebar, "dock_width": l.dock_width, "diff": format!("{:?}", l.diff), "columns": l.columns, "rows": l.rows, "tiled": l.tiled, "tabs": l.tabs, "completed": l.completed })
                });
                done(ControlResponse::ok(json!({
                    "active": self.active,
                    "plan_view": self.plan_view,
                    "resources": self.resources_json(),
                    "usage": {
                        "claude": self.usage.claude.chief_json(crate::usage::now()),
                        "codex": self.usage.codex.chief_json(crate::usage::now()),
                    },
                    "visible": self.visible_sessions().iter().map(|&i| &self.sessions[i].id).collect::<Vec<_>>(),
                    "chief": {
                        "open": self.chief_open, "dock": self.chief_dock(), "width": self.store.chief_width,
                        "prefer": self.config.chief.prefer,
                        "min_left_percent": self.config.chief.min_left_percent,
                    },
                    "window": viewport.map(|viewport| viewport.content),
                    "monitor": viewport.and_then(|viewport| viewport.monitor),
                    "layout": layout,
                    "sessions": sessions,
                    "update_blockers": self.update_blockers(),
                })))
            }
            ControlRequest::Open { session, path } => match self.find(&session) {
                None => missing(&session),
                Some(index) => {
                    let (id, root) = (self.sessions[index].id.clone(), self.sessions[index].root());
                    self.activate(&id, true);
                    self.open_file(&root, &path);
                    done(ControlResponse::ok(json!({ "opened": path })))
                }
            },
            ControlRequest::Edit { session, path, line } => match self.find(&session) {
                None => missing(&session),
                Some(index) => {
                    let (id, root) = (self.sessions[index].id.clone(), self.sessions[index].root());
                    self.activate(&id, true);
                    self.edit(&root, Some(&path), line);
                    done(ControlResponse::ok(json!({ "editing": path, "line": line })))
                }
            },
            ControlRequest::Wait { sessions, statuses, after, timeout_secs } => {
                let mut ids = Vec::new();
                for reference in &sessions {
                    match self.find(reference) {
                        Some(index) => ids.push(self.sessions[index].id.clone()),
                        None => {
                            let _ = reply.send(missing(reference));
                            return;
                        }
                    }
                }
                let statuses = if statuses.is_empty() {
                    ["turn done", "needs you", "exited", "reported", "ticket", "config"].map(String::from).to_vec()
                } else {
                    statuses
                };
                let timeout =
                    Duration::from_secs(timeout_secs.unwrap_or(atomics_core::control::DEFAULT_WAIT_SECS)).min(MAX_WAIT);
                self.waiters.push(Waiter {
                    sessions: ids,
                    statuses,
                    after: after.unwrap_or(self.event_seq),
                    deadline: Instant::now() + timeout,
                    reply,
                });
                self.serve_waiters();
                return;
            }
            ControlRequest::Result { session } => match self.find(&session) {
                None => missing(&session),
                Some(index) => {
                    // The pending request, and its id, as the screen shows it now: what approve will answer.
                    self.approvable(index);
                    let s = &self.sessions[index];
                    let screen = s.terminal.as_ref().map(|t| clip_start(&t.screen_text(40), MAX_SCREEN_CHARS));
                    let mut result = self.describe(index);
                    result["last_message"] = json!(s.last_message.as_deref().map(|m| clip(m, MAX_MESSAGE_CHARS)));
                    result["screen"] = json!(screen);
                    done(ControlResponse::ok(result))
                }
            },
            ControlRequest::PlanSet { plan, goal, mut tasks, integration, ticket } => {
                // Ids first: they must be unique, and newer links supersede pending creates of the slots
                // tasks will have, not of the ids sent.
                Plan::assign_task_ids(&mut tasks);
                let mut seen = std::collections::HashSet::new();
                if let Some(task) = tasks.iter().find(|task| !seen.insert(task.id.as_str())) {
                    let error = format!("Task id {} appears more than once: give each task its own id", task.id);
                    let _ = reply.send(done(ControlResponse::error(error)));
                    return;
                }
                let new = plan.is_none();
                let integration = integration.map(|branch| branch.trim().to_string()).filter(|b| !b.is_empty());
                // Checked as it would be written, before anything changes: a refused plan keeps the plan,
                // its approval and its links as they were.
                let mut candidate = if new {
                    Plan { id: plan::unique_id(&goal, &self.plans, &paths::atomics_home()), ..Plan::default() }
                } else {
                    self.plans[target].clone()
                };
                candidate.replace(goal.clone(), tasks.clone());
                if integration.is_some() {
                    candidate.integration = integration.clone();
                }
                candidate.project = self.plan_project(&candidate);
                let (refused, mut warnings) = self.plan_problems((!new).then_some(target), &candidate);
                self.usage.drain();
                warnings.extend(self.quota_warnings(&candidate));
                // One line per problem, so a plan of many tasks lists the first ones and counts the rest.
                if !refused.is_empty() {
                    let error = format!("Plan not saved:\n- {}", clip_list(&refused, 600, 20).join("\n- "));
                    let _ = reply.send(done(ControlResponse::error(error)));
                    return;
                }
                let index = if new {
                    self.plans.push(Plan { id: candidate.id, created_at: super::remote::now_ms(), ..Plan::default() });
                    self.plans.len() - 1
                } else {
                    target
                };
                let plan_id = self.plans[index].id.clone();
                for task in tasks.iter().filter(|t| t.ticket.is_some()) {
                    self.relinked((plan_id.clone(), Some(task.id.clone())));
                }
                if ticket.is_some() {
                    self.relinked((plan_id, None));
                }
                let tasks =
                    tasks.into_iter().map(|t| Task { ticket: self.stamp_system(t.ticket.clone()), ..t }).collect();
                // Writing a plan that finished reopens it: tasks, progress and branch stay, and the plan is
                // approved again only if the edit leaves its approved tasks as they were.
                if self.plans[index].status == plan::PlanStatus::Completed {
                    self.plans[index].reopen();
                }
                self.plans[index].replace(goal, tasks);
                let ticket = self.stamp_system(ticket);
                if ticket
                    .as_ref()
                    .is_some_and(|new| self.plans[index].ticket.as_ref().is_none_or(|old| !old.same_ticket(new)))
                {
                    self.plans[index].ticket_mode = None;
                }
                self.plans[index].ticket =
                    atomics_core::tickets::TicketLink::relink(ticket, self.plans[index].ticket.as_ref());
                if integration.is_some() {
                    self.plans[index].integration = integration;
                }
                self.plans[index].project = self.plan_project(&self.plans[index]);
                self.reload_repo_workflows();
                self.save_plan(index);
                self.remote_request_approval(index, false);
                let id = self.plans[index].id.clone();
                self.refresh_tickets(Some(&id), None);
                if new || (self.plan_view && self.selected_plan == index) {
                    self.selected_plan = index;
                    self.show_plan_fleet();
                    self.panel = PanelMode::Plan;
                    self.diff_open = true;
                } else {
                    self.changed_plans.insert(self.plans[index].id.clone());
                }
                let unknown = self.plans[index].unknown_dependencies();
                self.toast(if new {
                    "New plan: review and approve it on the plan board"
                } else if self.plans[index].approved {
                    "Plan updated: approval kept"
                } else {
                    "Plan updated: review and approve it on its board"
                });
                let mut result = json!({
                    "plan": self.plans[index].id,
                    "tasks": self.plans[index].tasks.len(),
                    "approved": self.plans[index].approved,
                    "integration": self.plans[index].integration_branch(),
                    "unknown_dependencies": unknown,
                    "next": if self.plans[index].approved { "Call plan_dispatch to start ready tasks." }
                        else { "The user reviews and approves the plan on the board; then call plan_dispatch." },
                });
                if !warnings.is_empty() {
                    result["warnings"] = json!(clip_list(&warnings, 600, 20));
                }
                done(ControlResponse::ok(result))
            }
            ControlRequest::PlanClose { plan: _, status } => match self.close_plan(target, status) {
                Ok(()) => done(ControlResponse::ok(self.plan_mutation_json(target, None))),
                Err(err) => done(ControlResponse::error(err)),
            },
            ControlRequest::PlanApprove { .. } => {
                self.approve_plan(target);
                done(ControlResponse::ok(self.plan_json(target)))
            }
            ControlRequest::PlanGet { plan: None, task: None } => done(ControlResponse::ok(json!({
                "plans": self.plans.iter().filter(|p| p.is_open()).map(Plan::summary).collect::<Vec<_>>()
            }))),
            ControlRequest::PlanGet { plan: None, task: Some(_) } => {
                done(ControlResponse::error("Pass plan with task: a task id is unique only within its plan."))
            }
            ControlRequest::PlanGet { plan: Some(id), task } => match self.plans.iter().position(|p| p.id == id) {
                Some(index) => done(match task {
                    None => ControlResponse::ok(self.plan_view(index)),
                    Some(task) => match self.task_view(index, &task) {
                        Ok(view) => ControlResponse::ok(view),
                        Err(error) => ControlResponse::error(error),
                    },
                }),
                None => done(ControlResponse::error(format!("no plan {id}"))),
            },
            ControlRequest::PlanUpdate { plan: _, id, status, note, session } => match self.plans[target].task_mut(&id)
            {
                None => done(ControlResponse::error(format!("no task {id}"))),
                Some(task) => {
                    let old = task.status;
                    let accepted = note.clone();
                    if let Some(status) = &status {
                        match TaskStatus::parse(status) {
                            Some(status) => task.set_status(status),
                            None => {
                                let _ = reply.send(done(ControlResponse::error(format!("unknown status {status}"))));
                                return;
                            }
                        }
                    }
                    if let Some(note) = note {
                        task.notes.push(note);
                    }
                    if let Some(reference) = session {
                        let found = self
                            .sessions
                            .iter()
                            .find(|s| s.id == reference || s.id.starts_with(&reference) || s.title == reference);
                        if let (Some(found), Some(task)) =
                            (found.map(|s| s.id.clone()), self.plans[target].task_mut(&id))
                        {
                            task.session = Some(found);
                            task.dispatched_at.get_or_insert_with(atomics_core::store::now);
                        }
                    }
                    self.finish_plan_agents();
                    self.save_plan(target);
                    if let Some(new) = self.plans[target].tasks.iter().find(|t| t.id == id).map(|t| t.status) {
                        let author = self.chief_author();
                        self.ticket_status_changed(target, &id, old, new, accepted.as_deref(), author);
                    }
                    done(ControlResponse::ok(self.plan_mutation_json(target, Some(&id))))
                }
            },
            ControlRequest::PlanStage { plan, task, stage, url, note } => {
                let result = self.stage_target(Some(&plan), task.as_deref(), None).and_then(|(index, task)| {
                    self.record_stage(
                        index,
                        task.as_deref(),
                        atomics_core::workflow::Stage { stage, url, note, ..Default::default() },
                        self.chief_author(),
                    )
                });
                done(match result {
                    Ok(value) => ControlResponse::ok(value),
                    Err(error) => ControlResponse::error(error),
                })
            }
            ControlRequest::StageEvent { plan, task, system, id, key, stage, url, note } => {
                let reference = (id.is_some() || key.is_some()).then(|| atomics_core::tickets::TicketLink {
                    system: system.unwrap_or_default(),
                    id: id.unwrap_or_default(),
                    key,
                    ..Default::default()
                });
                let result = self.stage_target(plan.as_deref(), task.as_deref(), reference.as_ref()).and_then(
                    |(index, task)| {
                        self.record_stage(
                            index,
                            task.as_deref(),
                            atomics_core::workflow::Stage { stage, url, note, ..Default::default() },
                            atomics_core::tickets::Author::Atomics,
                        )
                    },
                );
                done(match result {
                    Ok(value) => ControlResponse::ok(value),
                    Err(error) => ControlResponse::error(error),
                })
            }
            ControlRequest::PlanDispatch { plan: _, id } => {
                // It answers once the checks of the dependencies it waits on have.
                self.dispatch_when_checked(target, id, Some(reply));
                return;
            }
            ControlRequest::PlanIntegrate { plan: _, id } => {
                // Committing and merging can take a while: answer once it is done.
                self.integrate(target, &id, Some(reply));
                return;
            }
            ControlRequest::TicketLink { plan: _, task, ticket, create, unlink } => {
                self.link_ticket(target, task, ticket, create, unlink, reply);
                return;
            }
            ControlRequest::TicketSearch { query, semantic, take } => {
                self.chief_ticket(crate::tickets::TicketOp::Search { query, semantic }, take, reply);
                return;
            }
            ControlRequest::TicketRead { ticket } => {
                self.chief_ticket(crate::tickets::TicketOp::ReadCard(ticket.into_link()), None, reply);
                return;
            }
            ControlRequest::TicketChildren { ticket } => {
                self.chief_ticket(crate::tickets::TicketOp::Children(ticket.into_link()), None, reply);
                return;
            }
            ControlRequest::TicketRelated { ticket } => {
                self.chief_ticket(crate::tickets::TicketOp::Related(ticket.into_link()), None, reply);
                return;
            }
            ControlRequest::TicketSync { plan } => {
                self.sync_tickets(plan, reply);
                return;
            }
            ControlRequest::TicketComment { plan: _, task, text } => {
                self.comment_ticket(target, task, text, reply);
                return;
            }
            ControlRequest::TicketEvent { system, id, key, board, state, title, url, updated_at } => {
                let fields = atomics_core::tickets::TicketLink { state, title, url, updated_at, ..Default::default() };
                done(ControlResponse::ok(self.ticket_event(
                    system.as_deref(),
                    id.as_deref(),
                    key.as_deref(),
                    board.as_deref(),
                    fields,
                )))
            }
            ControlRequest::Doctor => {
                // The checks start programs and the ticket server: run them off the runtime thread.
                let workspace = self.workspace.clone();
                let projects = self.repos.clone();
                std::thread::spawn(move || {
                    let report = crate::doctor::json(workspace.as_deref(), &projects);
                    let _ = reply.send(ControlOutcome::Done(ControlResponse::ok(report)));
                });
                return;
            }
            ControlRequest::ConfigPropose { scope, toml, remove, theme, reason, pane } => {
                let change =
                    selfconfig::ProposedChange { scope, toml: &toml, remove: &remove, theme: theme.as_deref() };
                let pane = self.pane_session(pane);
                done(match self.propose_config(change, reason, pane.as_deref()) {
                    Ok(result) => ControlResponse::ok(result),
                    Err(error) => ControlResponse::error(error),
                })
            }
            ControlRequest::ConfigGet { project, key } => {
                done(match self.config_get(project.as_deref(), key.as_deref()) {
                    Ok(result) => ControlResponse::ok(result),
                    Err(error) => ControlResponse::error(error),
                })
            }
            ControlRequest::Preview { pane, port, path, label, remove } => {
                let result: Result<serde_json::Value, String> = (|| {
                    let pane = pane.ok_or_else(|| "preview needs an agent session".to_string())?;
                    let index = self.find(&pane).ok_or_else(|| "agent session not found".to_string())?;
                    if self.sessions[index].status == AgentStatus::Exited {
                        return Err("agent session ended".into());
                    }
                    // A sandboxed agent's servers listen inside its sandbox, and it may never name a port
                    // of this host for clients to open.
                    if self.sessions[index].sandbox.is_some() {
                        return Err("a sandboxed session cannot offer a preview of this host's ports".into());
                    }
                    let preview = atomics_core::preview::Preview { port, path, label };
                    preview.validate()?;
                    let ports = self.previews.declared.entry(self.sessions[index].id.clone()).or_default();
                    if !remove
                        && !ports.iter().any(|p| p.port == port)
                        && ports.len() >= atomics_core::preview::MAX_PREVIEWS
                    {
                        return Err("too many previews".into());
                    }
                    ports.retain(|p| p.port != port);
                    if !remove {
                        ports.push(preview);
                    }
                    Ok(json!({"port": port, "removed": remove}))
                })();
                match result {
                    Ok(result) => done(ControlResponse::ok(result)),
                    Err(error) => done(ControlResponse::error(error)),
                }
            }
            ControlRequest::Report { pane, status, summary } => {
                let pane = self.pane_session(pane);
                let task = self.report(pane.as_deref(), &status, &summary);
                done(ControlResponse::ok(json!({ "recorded": true, "task": task })))
            }
            ControlRequest::InlayHints { session } => match self.find(&session) {
                None => missing(&session),
                Some(index) => match self.editors.get(&self.sessions[index].root()) {
                    Some(editor) => {
                        editor.type_steps(self.config.editor.inlay_hints_keys.clone(), Duration::ZERO);
                        done(ControlResponse::ok(json!({ "toggled": true })))
                    }
                    None => done(ControlResponse::error("no editor open for that worktree")),
                },
            },
            ControlRequest::OpenProject { path } => {
                self.open_project(&paths::expand(&path));
                done(ControlResponse::ok(json!({ "projects": self.open_projects })))
            }
            ControlRequest::Cleanup { project } => match workspace::project_root(&paths::expand(&project)) {
                Ok(root) => {
                    self.open_cleanup(root.clone(), None);
                    done(ControlResponse::ok(json!({ "cleanup": root })))
                }
                Err(err) => done(ControlResponse::error(format!("{err:#}"))),
            },
            ControlRequest::ShowSettings { open } => {
                self.client_events.push(ClientEvent::ShowSettings(open));
                done(ControlResponse::ok(json!({ "settings_open": open })))
            }
            ControlRequest::ShowDiff { open } => {
                self.diff_open = open;
                done(ControlResponse::ok(json!({ "diff_open": open })))
            }
            ControlRequest::ShowChief { open, dock, width } => {
                if width.is_some_and(|w| !w.is_finite() || w < layout::MIN_DOCK_WIDTH) {
                    done(ControlResponse::error("chief width must be at least 520 points"))
                } else {
                    if let Some(dock) = dock {
                        self.set_chief_dock(dock);
                    }
                    if let Some(width) = width {
                        self.store.chief_width = Some(width);
                        self.save_chief_preferences();
                    }
                    if open && !self.chief_open {
                        self.toggle_chief();
                    } else if !open {
                        self.chief_open = false;
                    }
                    done(ControlResponse::ok(
                        json!({ "open": self.chief_open, "dock": self.chief_dock(), "width": self.store.chief_width }),
                    ))
                }
            }
            ControlRequest::Resize { width, height } => {
                self.client_events.push(ClientEvent::Viewport(egui::ViewportCommand::InnerSize(vec2(width, height))));
                done(ControlResponse::ok(json!({ "requested": [width, height] })))
            }
            ControlRequest::Start { agent, folder, repo, branch, prompt, args, isolation, interactive } => {
                // Control and remote requests start work on the host; they do not navigate the desktop.
                // A desktop start selects its own result when its reply comes back to that window.
                let selection = (self.active.clone(), self.diff_root.clone(), self.focus_terminal.clone());
                let started = match (folder, repo, branch) {
                    (_, Some(repo), Some(branch)) => {
                        // Checking out a big repository takes a while: do it off the UI thread
                        // and start the agent (and answer) once the worktree exists.
                        self.create_worktree(
                            paths::expand(&repo),
                            branch,
                            None,
                            Launch { agent, prompt, args, reply: Some(reply), task: None, isolation, interactive },
                        );
                        return;
                    }
                    (Some(folder), _, _) => {
                        self.interactive_start = interactive;
                        self.start_as(&agent, paths::expand(&folder), None, args, prompt, Role::Agent(isolation))
                    }
                    (None, Some(_), None) | (None, None, Some(_)) => {
                        Err("give a repo and a branch for a new worktree".to_string())
                    }
                    (None, None, None) => {
                        self.interactive_start = interactive;
                        self.start_scratch(&agent, args, prompt, isolation)
                    }
                };
                self.interactive_start = false;
                (self.active, self.diff_root, self.focus_terminal) = selection;
                match started {
                    Ok(id) => {
                        let index = self.find(&id).expect("just started");
                        done(ControlResponse::ok(self.describe(index)))
                    }
                    Err(err) => done(ControlResponse::error(err)),
                }
            }
            ControlRequest::Send { session, text, raw } => match self.find(&session) {
                None => missing(&session),
                // Only while atomics_approve can answer: any other menu takes keys.
                Some(index) if raw && self.approvable(index) => done(ControlResponse::error(
                    "raw input is blocked while permission is pending; use atomics_approve",
                )),
                Some(index) if raw => match &self.sessions[index].terminal {
                    Some(terminal) => {
                        terminal.write(unescape(&text).into_bytes());
                        done(ControlResponse::ok(json!({ "sent": true })))
                    }
                    None => done(ControlResponse::error("raw keys need a session running inside ATOMICS")),
                },
                Some(index) => done(match self.deliver(index, &text) {
                    Delivery::Pending => {
                        self.sessions[index].pending.as_mut().expect("pending delivery").reply = Some(reply);
                        return;
                    }
                    Delivery::Held => {
                        let reason = if self.sessions[index].approval_hold || !self.sessions[index].approvals.is_empty()
                        {
                            "permission answered"
                        } else {
                            "previous delivery confirmed"
                        };
                        ControlResponse::ok(json!({"sent":false, "held":true, "held_until":reason}))
                    }
                    Delivery::Sent | Delivery::Queued => ControlResponse::ok(
                        json!({"sent":true, "queued_until_next_hook":self.sessions[index].terminal.is_none()}),
                    ),
                    Delivery::Unavailable => {
                        ControlResponse::error("message not delivered: session exited or another delivery is pending")
                    }
                }),
            },
            ControlRequest::Read { session, lines } => match self.find(&session) {
                None => missing(&session),
                Some(index) => match &self.sessions[index].terminal {
                    Some(terminal) => done(ControlResponse::ok(json!({
                        "status": self.sessions[index].status.label(),
                        "screen": clip_start(&terminal.screen_text(lines.unwrap_or(60).min(MAX_READ_LINES)), MAX_SCREEN_CHARS),
                    }))),
                    None => done(ControlResponse::ok(json!({
                        "status": self.sessions[index].status.label(),
                        "last_prompt": self.sessions[index].last_prompt.as_deref().map(|p| clip(p, MAX_PROMPT_CHARS)),
                        "last_message": self.sessions[index].last_message.as_deref().map(|m| clip(m, MAX_MESSAGE_CHARS)),
                    }))),
                },
            },
            ControlRequest::Diff { session, patch, path } => match self.find(&session) {
                None => missing(&session),
                Some(index) => {
                    let root = self.sessions[index].root();
                    self.diffs.entry(root.clone()).or_default().mark_dirty();
                    let base_ref = self.project_config(&root).base_ref;
                    ControlOutcome::Diff { root, base_ref, patch, path, session: json!(self.sessions[index].id) }
                }
            },
            ControlRequest::Exec { session, command, timeout_secs, class } => match self.find(&session) {
                None => missing(&session),
                Some(index) => match self.exec(index, command, timeout_secs, class) {
                    Ok(outcome) => outcome,
                    Err(error) => done(ControlResponse::error(error)),
                },
            },
            ControlRequest::Comment { session, path, line, removed, text } => match self.find(&session) {
                None => missing(&session),
                Some(index) => {
                    let root = self.sessions[index].root();
                    let pending = self.add_comment(&root, path, line, removed, text);
                    done(ControlResponse::ok(json!({ "pending_comments": pending })))
                }
            },
            ControlRequest::SendReview { session } => match self.find(&session) {
                None => missing(&session),
                Some(index) => {
                    let root = self.sessions[index].root();
                    done(match self.deliver_review(&root) {
                        Ok(result) => ControlResponse::ok(result),
                        Err(error) => ControlResponse::error(error),
                    })
                }
            },
            ControlRequest::Activate { session } => match self.find(&session) {
                None => missing(&session),
                Some(index) => {
                    let id = self.sessions[index].id.clone();
                    // A session outside every plan is shown in the Ungrouped tab when the tab on screen hides it.
                    if !self.visible_sessions().contains(&index) && self.ungrouped_session(&self.sessions[index]) {
                        self.show_ungrouped();
                    }
                    self.activate(&id, true);
                    done(ControlResponse::ok(json!({ "active": id })))
                }
            },
            ControlRequest::Close { session } => match self.find(&session) {
                None => missing(&session),
                Some(index) => {
                    let id = self.sessions[index].id.clone();
                    self.close(&id);
                    done(ControlResponse::ok(json!({ "closed": id })))
                }
            },
            ControlRequest::Screenshot { path } => {
                self.client_events.push(ClientEvent::Screenshot { path, reply });
                return;
            }
        };
        let _ = reply.send(outcome);
    }

    /// `atomics_exec`: a command for a sandboxed session's sandbox, in its worktree, which the socket
    /// thread runs once a slot is free. A bare session's work is on this machine, where the chief
    /// runs its checks itself.
    fn exec(
        &self,
        index: usize,
        command: String,
        timeout_secs: Option<u64>,
        class: Option<SlotClass>,
    ) -> Result<ControlOutcome, String> {
        let s = &self.sessions[index];
        let Some(sandbox) = s.sandbox.clone() else {
            if self.pending_sandbox.iter().any(|(launch, _)| launch.id == s.id) {
                return Err(format!("{}'s sandbox is still starting; try again once its agent runs", s.title));
            }
            return Err(format!(
                "{} runs bare, on this machine, not in a sandbox: run the command on the host in {} (whole test \
                 suites through `atomics queue --class test -- <command>`)",
                s.title,
                s.root().display()
            ));
        };
        if command.trim().is_empty() {
            return Err("give a command to run".into());
        }
        let root = s.root();
        let job = crate::sandbox::exec::Exec {
            sbx: resolve_program(&self.sbx),
            sandbox: sandbox.clone(),
            // The sandbox mounts the worktree where its links lead.
            workdir: std::fs::canonicalize(&root).unwrap_or(root),
            command,
            timeout: crate::sandbox::exec::timeout(timeout_secs),
            class: class.unwrap_or(SlotClass::Test),
            ancestors: s.terminal.iter().map(|terminal| terminal.process_id()).collect(),
        };
        let (actions, id) = (self.action_tx.clone(), s.id.clone());
        let then: Box<dyn FnOnce() + Send> = Box::new(move || {
            let _ = actions.send(Box::new(move |state: &mut State| state.exec_finished(&id, sandbox)));
        });
        Ok(ControlOutcome::Exec { job, session: json!(s.id), then })
    }

    /// After a command in a sandbox: it ran agent code, which can move refs, so they are checked as
    /// after a turn. A sandbox it started for an agent that has gone stops again.
    pub(super) fn exec_finished(&mut self, id: &str, sandbox: String) {
        if let Some(index) = self.sessions.iter().position(|s| s.id == id) {
            self.check_refs(index);
        }
        let attached = self.sessions.iter().any(|s| {
            s.sandbox.as_deref() == Some(sandbox.as_str()) && s.terminal.as_ref().is_some_and(|t| !t.exited())
        }) || self.pending_sandbox.iter().any(
            |(launch, _)| matches!(&launch.runtime, AgentRuntime::Sandbox(sandboxed) if sandboxed.name == sandbox),
        );
        if !attached {
            self.stop_sandbox(sandbox);
        }
    }

    /// Records status changes as events and answers waiters they satisfy.
    pub(super) fn record_events(&mut self) {
        self.hub.lock().unwrap().chief_running = self.sessions.iter().any(|session| {
            session.chief
                && session.status != AgentStatus::Exited
                && session.terminal.as_ref().is_some_and(|terminal| !terminal.exited())
        });
        for session in &mut self.sessions {
            session.activity.observe(session.status);
            if self.last_status.get(&session.id) != Some(&session.status) {
                self.last_status.insert(session.id.clone(), session.status);
                self.event_seq += 1;
                self.events.push(StatusEvent {
                    seq: self.event_seq,
                    session: session.id.clone(),
                    plan: self.plans.iter().find(|p| p.contains_session(&session.id)).map(|p| p.id.clone()),
                    task: self
                        .plans
                        .iter()
                        .flat_map(|p| &p.tasks)
                        .find(|t| t.session.as_deref() == Some(&session.id))
                        .map(|t| t.id.clone()),
                    title: session.title.clone(),
                    status: session.status,
                    at: atomics_core::store::now(),
                    report: None,
                    approval: session.approvals.first().map(PendingApproval::json),
                    findings: None,
                    config: None,
                    ticket: None,
                    change: None,
                });
            }
        }
        if self.events.len() > 1000 {
            self.events.drain(..self.events.len() - 1000);
        }
        self.serve_waiters();
    }

    pub(super) fn serve_waiters(&mut self) {
        let now = Instant::now();
        let mut pending = Vec::new();
        for waiter in std::mem::take(&mut self.waiters) {
            let mut matching: Vec<(u64, serde_json::Value)> = self
                .events
                .iter()
                .filter(|e| e.seq > waiter.after)
                .filter(|e| {
                    e.findings.is_some()
                        || e.ticket.is_some()
                        || e.config.is_some()
                        || e.change.is_some()
                        || waiter.sessions.is_empty()
                        || waiter.sessions.contains(&e.session)
                })
                .filter(|e| waiter.statuses.iter().any(|s| s == e.kind()))
                .map(|e| (e.seq, e.to_json()))
                .collect();
            // Deliver the handoff even with a cursor from the previous process.
            if let Some(event) = self.restart_event.take() {
                matching.insert(0, (waiter.after, event));
            }
            let timed_out = now >= waiter.deadline;
            if matching.is_empty() && !timed_out {
                pending.push(waiter);
                continue;
            }
            // An answer carries the events that fit the client's limit, each with its texts cut. Its
            // cursor then stops at the last one, and the next wait reads on from there.
            let (mut events, mut size, mut read_to, mut more) = (Vec::new(), 0, waiter.after, false);
            for (seq, event) in matching {
                let event = clipped_event(event);
                let len = atomics_core::control::shown_len(&event);
                if !events.is_empty() && size + len > atomics_core::control::MAX_WAIT_CHARS {
                    more = true;
                    break;
                }
                size += len;
                read_to = read_to.max(seq);
                events.push(event);
            }
            let sessions: Vec<_> = self
                .sessions
                .iter()
                .map(|s| json!({ "id": s.id, "title": clip(&s.title, 120), "status": s.status.label() }))
                .collect();
            let mut response = json!({
                "events": events,
                "cursor": if more { read_to } else { self.event_seq },
                "timed_out": events.is_empty(),
                "sessions": sessions,
            });
            if more {
                response["more"] = json!("More events wait: call atomics_wait again with this cursor.");
            }
            let _ = waiter.reply.send(ControlOutcome::Done(ControlResponse::ok(response)));
        }
        self.waiters = pending;
    }
}

/// A `wait` event with its texts cut: a report summary to 8,000 characters, and an approval
/// request, ticket or config change to 2,000 characters per string and 50 items per list.
fn clipped_event(mut event: serde_json::Value) -> serde_json::Value {
    let summary = event.pointer_mut("/report/summary").map(serde_json::Value::take);
    atomics_core::control::clip_json(&mut event, 2000, 50);
    if let Some(summary) = summary {
        event["report"]["summary"] = json!(clip(summary.as_str().unwrap_or_default(), 8000));
    }
    event
}

/// `\r`, `\n`, `\t`, `\e` and `\xNN` escapes in raw key input.
fn unescape(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('e') => out.push('\x1b'),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                    out.push(byte as char);
                }
            }
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// Requests only a desktop window can serve fail clearly in the headless host instead of
/// queueing work nothing will consume.
fn headless_refusal(request: &ControlRequest) -> Option<&'static str> {
    match request {
        ControlRequest::Screenshot { .. } | ControlRequest::Resize { .. } | ControlRequest::ShowSettings { .. } => {
            Some(super::NO_WINDOW)
        }
        ControlRequest::Restart { .. } | ControlRequest::UpdateInstalled { .. } => {
            Some("the headless host does not restart itself; restart it with its service manager (systemctl restart)")
        }
        _ => None,
    }
}
