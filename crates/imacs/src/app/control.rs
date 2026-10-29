use super::*;

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
            "outside_imacs": s.terminal.is_none() && s.status != AgentStatus::Starting,
            "claude_session_id": s.claude_id,
            "last_prompt": s.last_prompt,
            "approval": s.approvals.first().map(|p| &p.request),
            "queue": s.terminal.as_ref().and_then(|t| {
                let (running, waiting) = self.governor.snapshot();
                queue_note(t.process_id(), &running, &waiting)
            }),
            "diff": stats,
        })
    }

    pub(super) fn control(&mut self, ctx: &egui::Context, request: ControlRequest, reply: Sender<ControlOutcome>) {
        if self.restarting {
            let _ = reply.send(ControlOutcome::Done(ControlResponse::error("IMACS is restarting")));
            return;
        }
        let done = |response: ControlResponse| ControlOutcome::Done(response);
        let missing = |reference: &str| done(ControlResponse::error(format!("no session matches {reference:?}")));
        let target = match &request {
            ControlRequest::PlanSet { plan: Some(id), .. } | ControlRequest::PlanClose { plan: id, .. } => {
                Some(Some(id.as_str()))
            }
            ControlRequest::PlanUpdate { plan, .. }
            | ControlRequest::PlanDispatch { plan, .. }
            | ControlRequest::PlanIntegrate { plan, .. }
            | ControlRequest::TicketLink { plan, .. }
            | ControlRequest::TicketComment { plan, .. } => Some(plan.as_deref()),
            _ => None,
        };
        let target = if let Some(id) = target {
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
                self.agent_ticket(pane.as_deref(), plan, None, reply);
                return;
            }
            ControlRequest::AgentTicketComment { pane, text } => {
                self.agent_ticket(pane.as_deref(), false, Some(text), reply);
                return;
            }
            ControlRequest::Restart { force, reason } => done(self.restart(force, reason)),
            ControlRequest::Approve { session, decision, message } => match self.find(&session) {
                Some(index) => {
                    let approval =
                        self.sessions[index].approvals.first().map(|pending| pending.id.clone()).unwrap_or_default();
                    done(match self.approve(index, &approval, decision, message.as_deref()) {
                        Ok(()) => ControlResponse::ok(json!({"answered":true})),
                        Err(error) => ControlResponse::error(error),
                    })
                }
                None => missing(&session),
            },
            ControlRequest::List => {
                let sessions: Vec<_> = (0..self.sessions.len()).map(|i| self.describe(i)).collect();
                let (window, monitor) = ctx.input(|i| (i.content_rect().size(), i.viewport().monitor_size));
                let layout = self.last_layout.map(|l| {
                    json!({ "sidebar": l.sidebar, "dock_width": l.dock_width, "diff": format!("{:?}", l.diff), "columns": l.columns, "rows": l.rows, "tiled": l.tiled, "tabs": l.tabs, "completed": l.completed })
                });
                done(ControlResponse::ok(json!({
                    "active": self.active,
                    "plan_view": self.plan_view,
                    "resources": self.resources_json(),
                    "visible": self.visible_sessions().iter().map(|&i| &self.sessions[i].id).collect::<Vec<_>>(),
                    "chief": { "open": self.chief_open, "dock": self.chief_dock(), "width": self.store.chief_width },
                    "window": [window.x, window.y],
                    "monitor": monitor.map(|m| [m.x, m.y]),
                    "layout": layout,
                    "sessions": sessions,
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
                    self.edit(ctx, &root, Some(&path), line);
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
                    vec!["turn done".into(), "needs you".into(), "exited".into(), "reported".into(), "ticket".into()]
                } else {
                    statuses
                };
                let timeout = Duration::from_secs(timeout_secs.unwrap_or(120)).min(MAX_WAIT);
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
                    let s = &self.sessions[index];
                    let screen = s.terminal.as_ref().map(|t| t.screen_text(40));
                    let mut result = self.describe(index);
                    result["last_message"] = json!(s.last_message);
                    result["screen"] = json!(screen);
                    done(ControlResponse::ok(result))
                }
            },
            ControlRequest::PlanSet { plan, goal, mut tasks, integration, ticket } => {
                let new = plan.is_none();
                let index = if plan.is_some() {
                    target
                } else {
                    let id = plan::unique_id(&goal, &self.plans, &paths::imacs_home());
                    self.plans.push(Plan { id, ..Plan::default() });
                    self.plans.len() - 1
                };
                let plan_id = self.plans[index].id.clone();
                // Newer links supersede pending creates of the slots tasks will have, not of the ids sent.
                Plan::assign_task_ids(&mut tasks);
                for task in tasks.iter().filter(|t| t.ticket.is_some()) {
                    self.relinked((plan_id.clone(), Some(task.id.clone())));
                }
                if ticket.is_some() {
                    self.relinked((plan_id, None));
                }
                let tasks =
                    tasks.into_iter().map(|t| Task { ticket: self.stamp_system(t.ticket.clone()), ..t }).collect();
                self.plans[index].replace(goal, tasks);
                let ticket = self.stamp_system(ticket);
                self.plans[index].ticket =
                    imacs_core::tickets::TicketLink::relink(ticket, self.plans[index].ticket.as_ref());
                if let Some(branch) = integration.filter(|b| !b.trim().is_empty()) {
                    self.plans[index].integration = Some(branch.trim().to_string());
                }
                let project = self.plans[index]
                    .tasks
                    .iter()
                    .find_map(|t| t.repo.as_deref())
                    .map(paths::expand)
                    .or_else(|| self.plans[index].project.clone())
                    .or_else(|| {
                        self.diff_target().and_then(|t| self.project_of(&t)).or_else(|| self.repos.first().cloned())
                    });
                self.plans[index].project = project.and_then(|p| self.project_of(&p));
                self.save_plan(index);
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
                } else {
                    "Plan updated: review and approve it on its board"
                });
                done(ControlResponse::ok(json!({
                    "plan": self.plans[index].id,
                    "tasks": self.plans[index].tasks.len(),
                    "approved": false,
                    "integration": self.plans[index].integration_branch(),
                    "unknown_dependencies": unknown,
                    "next": "The user reviews and approves the plan on the board; then call plan_dispatch.",
                })))
            }
            ControlRequest::PlanClose { plan: _, status } => match self.close_plan(target, status) {
                Ok(()) => done(ControlResponse::ok(self.plan_mutation_json(target, None))),
                Err(err) => done(ControlResponse::error(err)),
            },
            ControlRequest::PlanGet { plan: None } => done(ControlResponse::ok(json!({
                "plans": self.plans.iter().filter(|p| p.is_open()).map(Plan::summary).collect::<Vec<_>>()
            }))),
            ControlRequest::PlanGet { plan: Some(id) } => match self.plans.iter().position(|p| p.id == id) {
                Some(index) => done(ControlResponse::ok(self.plan_json(index))),
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
                            task.dispatched_at.get_or_insert_with(imacs_core::store::now);
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
            ControlRequest::PlanDispatch { plan: _, id } => match self.dispatch(target, id.as_deref()) {
                Ok(started) => done(ControlResponse::ok(json!({ "started": started }))),
                Err(err) => done(ControlResponse::error(err)),
            },
            ControlRequest::PlanIntegrate { plan: _, id } => {
                // Committing and merging can take a while: answer once it is done.
                self.integrate(target, &id, Some(reply));
                return;
            }
            ControlRequest::TicketLink { plan: _, task, ticket, create, unlink } => {
                self.link_ticket(target, task, ticket, create, unlink, reply);
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
                let fields = imacs_core::tickets::TicketLink { state, title, url, updated_at, ..Default::default() };
                done(ControlResponse::ok(self.ticket_event(
                    system.as_deref(),
                    id.as_deref(),
                    key.as_deref(),
                    board.as_deref(),
                    fields,
                )))
            }
            ControlRequest::Report { pane, status, summary } => {
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
                        self.toggle_chief(ctx);
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
            ControlRequest::Start { agent, folder, repo, branch, prompt, args } => {
                let folder = match (folder, repo, branch) {
                    (_, Some(repo), Some(branch)) => {
                        // Checking out a big repository takes a while: do it off the UI thread
                        // and start the agent (and answer) once the worktree exists.
                        self.create_worktree(
                            paths::expand(&repo),
                            branch,
                            None,
                            Launch { agent, prompt, args, reply: Some(reply), task: None },
                        );
                        return;
                    }
                    (Some(folder), _, _) => Ok(paths::expand(&folder)),
                    _ => Err("give a folder, or a repo and a branch for a new worktree".to_string()),
                };
                match folder.and_then(|folder| self.start(ctx, &agent, folder, None, args, prompt)) {
                    Ok(id) => {
                        let index = self.find(&id).expect("just started");
                        done(ControlResponse::ok(self.describe(index)))
                    }
                    Err(err) => done(ControlResponse::error(err)),
                }
            }
            ControlRequest::Send { session, text, raw } => match self.find(&session) {
                None => missing(&session),
                Some(index)
                    if raw && (self.sessions[index].approval_hold || !self.sessions[index].approvals.is_empty()) =>
                {
                    done(ControlResponse::error("raw input is blocked while permission is pending; use imacs_approve"))
                }
                Some(index) if raw => match &self.sessions[index].terminal {
                    Some(terminal) => {
                        terminal.write(unescape(&text).into_bytes());
                        done(ControlResponse::ok(json!({ "sent": true })))
                    }
                    None => done(ControlResponse::error("raw keys need a session running inside IMACS")),
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
                        "screen": terminal.screen_text(lines.unwrap_or(60)),
                    }))),
                    None => done(ControlResponse::ok(json!({
                        "status": self.sessions[index].status.label(),
                        "last_prompt": self.sessions[index].last_prompt,
                        "last_message": self.sessions[index].last_message,
                    }))),
                },
            },
            ControlRequest::Diff { session, patch } => match self.find(&session) {
                None => missing(&session),
                Some(index) => {
                    let root = self.sessions[index].root();
                    self.diffs.entry(root.clone()).or_default().mark_dirty();
                    ControlOutcome::Diff {
                        root,
                        base_ref: self.config.base_ref.clone(),
                        patch,
                        session: json!(self.sessions[index].id),
                    }
                }
            },
            ControlRequest::Comment { session, path, line, removed, text } => match self.find(&session) {
                None => missing(&session),
                Some(index) => {
                    let root = self.sessions[index].root();
                    let state = self.diffs.entry(root.clone()).or_default();
                    let file = state.prepared.as_ref().and_then(|p| p.diff.files.iter().find(|f| f.path() == path));
                    let excerpt = file
                        .and_then(|f| {
                            f.hunks.iter().flat_map(|h| &h.lines).find(|l| {
                                let number = if removed { l.old_no } else { l.new_no };
                                line.is_some()
                                    && number == line
                                    && (l.kind == imacs_core::diff::LineKind::Removed) == removed
                            })
                        })
                        .map(|l| l.text.clone())
                        // Untracked content a diff off screen left out is read from the file.
                        .or_else(|| {
                            let n = line.filter(|_| !removed && file.is_some_and(|f| f.omitted > 0))?;
                            let text = std::fs::read_to_string(root.join(&path)).ok()?;
                            text.lines().nth((n as usize).checked_sub(1)?).map(str::to_string)
                        })
                        .unwrap_or_default();
                    state.comments.push(steer::ReviewComment { path, line, removed, excerpt, body: text });
                    done(ControlResponse::ok(json!({ "pending_comments": state.comments.len() })))
                }
            },
            ControlRequest::SendReview { session } => match self.find(&session) {
                None => missing(&session),
                Some(index) => {
                    let root = self.sessions[index].root();
                    let count = self.diffs.get(&root).map(|d| d.comments.len()).unwrap_or(0);
                    if count == 0 {
                        done(ControlResponse::error("no pending comments"))
                    } else {
                        let delivery = self.send_review(&root);
                        let left = self.diffs.get(&root).map(|d| d.comments.len()).unwrap_or(0);
                        if left == 0 {
                            done(ControlResponse::ok(if delivery == Delivery::Held {
                                json!({"held_comments":count, "held":true})
                            } else if delivery == Delivery::Pending {
                                json!({"pending_comments":count, "sent":false})
                            } else {
                                json!({ "sent_comments": count })
                            }))
                        } else {
                            done(ControlResponse::error("could not deliver the review"))
                        }
                    }
                }
            },
            ControlRequest::Activate { session } => match self.find(&session) {
                None => missing(&session),
                Some(index) => {
                    let id = self.sessions[index].id.clone();
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
                    at: imacs_core::store::now(),
                    report: None,
                    approval: session.approvals.first().map(|pending| pending.request.clone()),
                    findings: None,
                    ticket: None,
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
            let mut matching: Vec<serde_json::Value> = self
                .events
                .iter()
                .filter(|e| e.seq > waiter.after)
                .filter(|e| {
                    e.findings.is_some()
                        || e.ticket.is_some()
                        || waiter.sessions.is_empty()
                        || waiter.sessions.contains(&e.session)
                })
                .filter(|e| waiter.statuses.iter().any(|s| s == e.kind()))
                .map(StatusEvent::to_json)
                .collect();
            // Deliver the handoff even with a cursor from the previous process.
            if let Some(event) = self.restart_event.take() {
                matching.insert(0, event);
            }
            let timed_out = now >= waiter.deadline;
            if matching.is_empty() && !timed_out {
                pending.push(waiter);
                continue;
            }
            let sessions: Vec<_> = self
                .sessions
                .iter()
                .map(|s| json!({ "id": s.id, "title": s.title, "status": s.status.label() }))
                .collect();
            let response = json!({ "events": matching, "cursor": self.event_seq, "timed_out": matching.is_empty(), "sessions": sessions });
            let _ = waiter.reply.send(ControlOutcome::Done(ControlResponse::ok(response)));
        }
        self.waiters = pending;
    }
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
