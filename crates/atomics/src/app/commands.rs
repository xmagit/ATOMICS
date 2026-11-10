//! Commands, as the desktop carries them out. The draw code, moved or not, says what the user asks for
//! with [`Command`]s; [`Desktop::send`] hands a host's command to the in-process [`Backend`], whose
//! [`Local::run`] posts it as an action that runs on the runtime thread, in turn with the host's other
//! work. IDs are resolved when the host runs them, so a session or plan disappearing between paint and
//! execution is safe.
use atomics_core::ops::{self, ApprovalDecide, Outcome};
use atomics_ui::Command;
use atomics_ui::terminal_surface::TerminalSurface;

use super::backend::Local;
use super::fleet::Terminals;
use super::*;

pub(super) type Action = Box<dyn FnOnce(&mut State) + Send>;

impl State {
    pub(super) fn drain_actions(&mut self) -> bool {
        let mut changed = false;
        while let Ok(action) = self.action_rx.try_recv() {
            action(self);
            changed = true;
        }
        changed
    }

    pub(super) fn post(&self, action: impl FnOnce(&mut State) + Send + 'static) {
        let _ = self.action_tx.send(Box::new(action));
    }

    /// Posts `action` for the plan at `index`, found again by its id when it runs.
    pub(super) fn post_plan(&self, index: usize, action: impl FnOnce(&mut State, usize) + Send + 'static) {
        let id = self.plans[index].id.clone();
        self.post(move |state| {
            if let Some(index) = state.plans.iter().position(|p| p.id == id) {
                action(state, index);
            }
        });
    }

    pub(super) fn post_session(&self, index: usize, action: impl FnOnce(&mut State, usize) + Send + 'static) {
        let id = self.sessions[index].id.clone();
        self.post(move |state| {
            if let Some(index) = state.find(&id) {
                action(state, index);
            }
        });
    }

    /// Posts a task card's `action` for the task at `task` of the plan at `plan`, both found again by
    /// id when it runs.
    fn post_task(&self, plan: usize, task: usize, action: &'static str) {
        let (plan, task) = (self.plans[plan].id.clone(), self.plans[plan].tasks[task].id.clone());
        self.post(move |state| {
            if let Some(plan) = state.plans.iter().position(|p| p.id == plan)
                && let Some(task) = state.plans[plan].tasks.iter().position(|t| t.id == task)
            {
                state.task_action(plan, task, action);
            }
        });
    }
}

impl Local<'_> {
    /// Carries out a host's command with the posts and `State` methods the desktop's code used before
    /// commands.
    pub(super) fn run(&self, command: Command) {
        let state = self.0;
        match command {
            Command::OpenPreview { preview, .. } => {
                if preview.validate().is_ok() {
                    // The TLS probe and the browser launch run outside the runtime and its lock.
                    std::thread::spawn(move || {
                        let tls = crate::preview::client::speaks_tls(preview.port);
                        let _ = crate::preview::client::open_browser(&preview.url_with(preview.port, tls));
                    });
                }
            }
            Command::StopPreview { .. } => {}
            Command::EditorHints(root) => state.post(move |state| {
                if let Some(editor) = state.editors.get(&root) {
                    editor.type_steps(state.config.editor.inlay_hints_keys.clone(), Duration::ZERO);
                }
            }),
            Command::LoadPullRequest(key) => state.post(move |state| {
                state.load_pull_request(&key);
            }),
            // The desktop's decision goes through the journal and `State::decide` like a protocol
            // client's, so the two can race and exactly one wins.
            Command::Decide { session, approval, decision } => {
                let session = state.sessions[session].id.clone();
                state.post(move |state| {
                    let outcome = state.mutate_desktop(|op| {
                        let decide = ApprovalDecide { op, approval: approval.clone(), decision, message: None };
                        ops::Command::ApprovalDecide(decide)
                    });
                    if let Outcome::Refused { message, .. } = outcome {
                        state.toast(message);
                    }
                    state.client_events.push(ClientEvent::ApprovalDecided { session, approval });
                });
            }
            Command::EditBrief { plan, task, brief } => {
                state.post_plan(plan, move |state, index| state.edit_task_brief(index, &task, brief))
            }
            Command::CurateNote { plan, id, pinned, delete } => state.post_plan(plan, move |state, index| {
                if let Err(error) = state.curate_notebook(index, id, pinned, delete) {
                    state.toast(error);
                }
            }),
            Command::StartTask { plan, task } => {
                state.post_plan(plan, move |state, index| state.dispatch_when_checked(index, Some(task), None))
            }
            Command::IntegrateTask { plan, task } => {
                state.post_plan(plan, move |state, index| state.integrate(index, &task, None))
            }
            Command::ResumeTask { plan, task } => state.post_task(plan, task, "resume"),
            Command::AcknowledgeRefs { plan, task } => state.post_task(plan, task, "acknowledge_refs"),
            Command::SetTaskStatus { plan, task, status } => state.post_plan(plan, move |state, index| {
                if state.plans[index].status != PlanStatus::Open {
                    return;
                }
                let old = state.plans[index].task_mut(&task).map(|task| {
                    let old = task.status;
                    task.set_status(status);
                    old
                });
                state.finish_plan_agents();
                state.save_plan(index);
                if let Some(old) = old {
                    let author = atomics_core::tickets::Author::User;
                    state.ticket_status_changed(index, &task, old, status, None, author);
                }
            }),
            Command::ApprovePlan(plan) => state.post_plan(plan, |state, index| {
                state.approve_plan(index);
                state.toast("Plan approved: the orchestrator can dispatch it now");
            }),
            Command::IntegrateAccepted(plan) => state.post_plan(plan, |state, index| {
                let ids: Vec<_> = state.plans[index].pending_integrations().iter().map(|t| t.id.clone()).collect();
                for id in ids {
                    state.integrate(index, &id, None);
                }
            }),
            Command::SyncTickets(plan) => state.post_plan(plan, |state, index| {
                let id = state.plans[index].id.clone();
                state.resync_tickets(&id);
            }),
            Command::DispatchReady(plan) => {
                state.post_plan(plan, |state, index| state.dispatch_when_checked(index, None, None))
            }
            Command::ReopenPlan(plan) => state.post_plan(plan, |state, index| {
                if let Err(error) = state.reopen_plan(index) {
                    state.toast(error);
                }
            }),
            Command::ArchivePlan(plan) => state.post_plan(plan, |state, index| {
                if let Err(error) = state.close_plan(index, PlanStatus::Archived) {
                    state.toast(error);
                }
            }),
            Command::ClosePlan { plan, status } => state.post_plan(plan, move |state, index| {
                if let Err(error) = state.close_plan(index, status) {
                    state.toast(error);
                }
            }),
            Command::ArchiveFinished(plan) => state.post_plan(plan, |state, index| {
                if state.plans[index].status != PlanStatus::Open
                    && let Err(error) = state.close_plan(index, PlanStatus::Archived)
                {
                    state.toast(error);
                }
            }),
            Command::EditComments { root, before, after } => state.post(move |state| {
                state.diffs.entry(root).or_default().edit_comments(before, after);
            }),
            Command::SendReview(root) => state.post(move |state| {
                let _ = state.send_review(&root);
            }),
            Command::RefreshDiff(root) => state.post(move |state| state_refresh(state.diffs.entry(root).or_default())),
            Command::OpenFile { root, path } => state.post(move |state| state.open_file(&root, &path)),
            Command::LeaveFile(root) => state.post(move |state| {
                let data = state.diffs.entry(root).or_default();
                data.viewing = None;
                data.file = None;
                data.file_error = None;
            }),
            Command::Edit { root, path, line } => state.post(move |state| state.edit(&root, path.as_deref(), line)),
            Command::Steer { sessions, text } => state.post(move |state| state.steer_sessions(sessions, text)),
            Command::StartAgent { agent, folder } => state.post(move |state| state.pending_open.push((agent, folder))),
            Command::Start { agent, folder } => {
                state.post(move |state| state.client_commands.push(start_request(agent, folder)))
            }
            Command::CloseSession(id) => state.post(move |state| state.close(&id)),
            Command::Resume(record) => state.post(move |state| state.resume.push(record)),
            Command::ResumeSession(id) => state.post(move |state| state.resume_exited(&id)),
            Command::RenameSession { session, title } => state.post(move |state| {
                if let Some(found) = state.sessions.iter_mut().find(|s| s.id == session) {
                    found.rename(&title);
                }
                state.remember(&session);
            }),
            Command::CloseIdle(root) => state.post(move |state| {
                let closed = state.close_idle(&root);
                state.toast(format!("Closed {closed} idle session{}", if closed == 1 { "" } else { "s" }));
            }),
            Command::SeeSession(id) => state.post(move |state| {
                if let Some(session) = state.sessions.iter_mut().find(|s| s.id == id)
                    && session.status == AgentStatus::Done
                {
                    session.status = AgentStatus::Idle;
                }
            }),
            Command::DismissMcp(id) => state.post(move |state| {
                if let Some(index) = state.find(&id) {
                    state.sessions[index].mcp_needed.clear();
                }
            }),
            Command::VoiceToSession { session, text } => {
                let quoted = format!("“{}”", truncate(&text, 60));
                state.post_session(session, move |state, index| {
                    if state.deliver(index, &text) != Delivery::Unavailable {
                        state.toast(format!("Sent to {}: {quoted}", state.sessions[index].title));
                    } else {
                        let error = "No active agent to send to".into();
                        state.client_events.push(ClientEvent::VoiceFailed { text, error });
                    }
                });
            }
            Command::Manage(action) => state.post(move |state| {
                let outcome = state.mutate_desktop(|op| {
                    atomics_core::ops::Command::HostCommand(atomics_core::ops::HostCommand { op, action })
                });
                if let atomics_core::ops::Outcome::Refused { message, .. } = outcome {
                    state.toast(message);
                }
            }),
            Command::CopySecret(name) => {
                // No ctl, MCP, protocol, or connected native client can construct this command
                // on the credential host. Check the actual window again before OS authentication.
                if !state.window.is_attached() {
                    return;
                }
                let keychain = state.relay.keychain().clone();
                let actions = state.action_tx.clone();
                std::thread::spawn(move || {
                    let message = match crate::secret_copy::copy(keychain, &name) {
                        Ok(()) => "Secret copied after OS authentication".to_string(),
                        Err(error) => error.to_string(),
                    };
                    let _ = actions.send(Box::new(move |state| {
                        state.credential_status["copy_note"] = json!(message);
                        state.toast(message);
                    }));
                });
            }
            Command::OpenProject(root) => state.post(move |state| state.open_project(&root)),
            Command::CloseProject(root) => state.post(move |state| state.close_project(&root)),
            Command::OpenWorkspace(name) => state.post(move |state| {
                let _ = state.open_workspace(&name);
            }),
            Command::Ticket { request, action } => state.post(move |state| {
                if let Err(error) = state.ticket_ui_command(request, action) {
                    state.toast(error);
                }
            }),
            Command::SignInTickets => state.post(State::start_sign_in),
            Command::PasteSignIn(text) => state.post(move |state| {
                if let Err(error) = state.paste_signin(&text) {
                    state.toast(error);
                }
            }),
            Command::CancelSignIn => state.post(|state| {
                let _ = state.cancel_sign_in();
            }),
            Command::SaveWorkspace(name) => state.post(move |state| state.save_workspace(&name)),
            Command::OpenCleanup { repo, preselect } => state.post(move |state| state.open_cleanup(repo, preselect)),
            Command::CleanupFetch => state.post(|state| state.cleanup_action("fetch")),
            Command::CleanupPrune => state.post(|state| state.cleanup_action("prune")),
            Command::CleanupReload => state.post(|state| state.cleanup_action("reload")),
            Command::RemoveWorktrees { worktrees, orphans, delete_branches } => {
                state.post(move |state| state.remove_worktrees(&worktrees, orphans, delete_branches))
            }
            Command::ToggleChief => state.post(|state| state.toggle_chief()),
            Command::SetChiefDock(dock) => state.post(move |state| state.set_chief_dock(dock)),
            Command::SaveChiefPreferences => state.post(State::save_chief_preferences),
            Command::VoiceToChief(text) => {
                let quoted = format!("“{}”", truncate(&text, 60));
                state.post(move |state| match state.send_to_chief(&text) {
                    Ok(()) => state.toast(format!("Sent to the chief: {quoted}")),
                    Err(error) => state.client_events.push(ClientEvent::VoiceFailed { text, error }),
                })
            }
            Command::EditConfig => state.post(State::edit_config),
            Command::CheckSandbox => state.post(|state| state.check_sandbox()),
            Command::Update => state.post(State::start_update),
            Command::CancelUpdate => state.post(State::cancel_update_wait),
            Command::RefreshConnections => state.post(|state| state.refresh_connections()),
            Command::ImportConnections => state.post(|state| state.import_connections()),
            Command::Connect { name, store_env, url } => {
                state.post(move |state| state.connect_server(&name, store_env, url))
            }
            Command::Disconnect(name) => state.post(move |state| state.disconnect_server(&name)),
            Command::AddConnection { name, url } => state.post(move |state| {
                if let Err(error) = state.add_connection(&name, &url) {
                    state.toast(error);
                }
            }),
            Command::DecideProposal { id, apply, note } => state.post(move |state| {
                if apply {
                    state.apply_proposal(&id, note);
                } else {
                    state.reject_proposal(&id, note);
                }
            }),
            Command::CleanUpPlan(plan) => state.post_plan(plan, |state, index| state.open_plan_cleanup(index)),
            Command::ReloadPlanCleanup => state.post(State::reload_plan_cleanup),
            Command::RemovePlanBranches { plan, branches } => {
                state.post(move |state| state.remove_plan_branches(&plan, &branches))
            }
            // The UI carries out its own view commands.
            command => debug_assert!(command.is_view(), "Local does not run {command:?}"),
        }
    }
}

/// `atomics ctl start` for an agent in `folder`, or with none in a scratch folder of its own.
fn start_request(agent: String, folder: Option<PathBuf>) -> ControlRequest {
    ControlRequest::Start {
        agent,
        folder: folder.map(|folder| folder.to_string_lossy().into_owned()),
        repo: None,
        branch: None,
        prompt: None,
        args: Vec::new(),
        isolation: None,
        interactive: true,
    }
}

impl Desktop<'_> {
    /// Draws with atomics-ui code: the client's palette is the host's theme, reads go through the
    /// in-process backend, and the commands the draw recorded run when it returns.
    pub(super) fn draw<R>(&mut self, draw: impl FnOnce(&dyn Backend, &mut atomics_ui::Client) -> R) -> R {
        self.draw_terminals(|backend, client, _| draw(backend, client))
    }

    /// [`Desktop::draw`] for views that hold terminals: they draw each through the desktop's own terminal
    /// views ([`Terminals`]).
    pub(super) fn draw_terminals<R>(
        &mut self,
        draw: impl FnOnce(&dyn Backend, &mut atomics_ui::Client, &mut dyn TerminalSurface) -> R,
    ) -> R {
        if self.runtime.github.pr_diff.is_some() && self.runtime.github_pr_diff().is_none() {
            self.client.ui.pr_view = None;
        }
        self.client.ui.palette = self.palette();
        let mut terminals = Terminals {
            sessions: &self.runtime.sessions,
            editors: &self.runtime.editors,
            views: &mut self.client.terminal_views,
        };
        let result = draw(&self.runtime.backend(), &mut self.client.ui, &mut terminals);
        for command in std::mem::take(&mut self.client.ui.commands) {
            self.send(command);
        }
        result
    }

    /// Carries out a command now: a view command ([`Command::is_view`]) on this window, any other through
    /// the in-process backend. Selection is immediate client state, projected into the native model
    /// (`active`, `diff_root`, `diff_open`, `panel`, the tab) for ctl and the host's subscription and
    /// scheduling decisions; dialogs open on this window's client. The welcome's [`Command::Start`] joins
    /// the window's control requests, which go to the host when the frame is drawn.
    pub(super) fn send(&mut self, command: Command) {
        match command {
            Command::Activate { session, open_diff } => self.activate(&session, open_diff),
            Command::OpenTaskSession(session) => {
                self.activate(&session, true);
                self.runtime.panel = PanelMode::Diff;
            }
            Command::ShowTaskDiff { plan, task } => self.runtime.task_action(plan, task, "diff"),
            Command::ShowRoot(root) => {
                self.runtime.active = None;
                self.runtime.diff_root = Some(root);
                self.runtime.diff_open = true;
            }
            Command::OpenDiff => self.runtime.diff_open = true,
            Command::CloseDiff => self.runtime.diff_open = false,
            Command::ToggleDiff => self.runtime.diff_open = !self.runtime.diff_open,
            Command::SetPanel(panel) => self.runtime.panel = panel,
            Command::SetView(view) => self.runtime.set_view(view),
            Command::ShowUngrouped => self.runtime.show_ungrouped(),
            Command::ShowPlan(plan) => {
                self.runtime.selected_plan = plan;
                self.runtime.show_plan_fleet();
            }
            Command::ViewPlanHistory(plan) => self.view_plan_history(plan),
            Command::TogglePlan => self.runtime.toggle_plan(),
            Command::ToggleView(view) => {
                self.runtime.toggle_view(view);
                // The Tickets panel takes the keyboard where it opens; the terminal gets it back where it closes.
                if view == PanelMode::Tickets {
                    if self.runtime.panel == PanelMode::Tickets && self.runtime.diff_open {
                        self.client.focus_terminal = None;
                        self.client.ui.focus_editor = false;
                    } else {
                        self.client.focus_terminal = self.runtime.active.clone();
                    }
                }
            }
            Command::OpenTicket(link) => {
                self.runtime.open_ticket(link, tickets_panel::Nav::Fresh);
                self.runtime.tickets.panel.from_board = true;
            }
            Command::EditingPlan => self.runtime.plan_edit_open = true,
            Command::ClosePlanTab(plan) => self.close_plan_tab(plan),
            Command::NewSession => self.open_new_dialog(None),
            Command::NewWorktree(root) => {
                self.open_new_dialog(Some(root.clone()));
                if let Some(dialog) = &mut self.client.new_dialog {
                    dialog.place = Place::Worktree;
                    dialog.repo = paths::display(&root);
                }
            }
            Command::Rename(session) => self.begin_rename(&session),
            Command::PickProject => {
                self.client.ui.management.project = true;
                self.client.ui.management.folder_page = 0;
                let start = paths::home().to_string_lossy().into_owned();
                self.client.ui.management.folder = Some(start.clone());
                self.send(Command::Manage(atomics_core::ops::HostAction::BrowseFolder { path: start }));
            }
            Command::HideChief => self.runtime.chief_open = false,
            Command::ResizeChief(width) => self.runtime.store.chief_width = width,
            Command::Toast(text) => self.runtime.toast(text),
            Command::ConnectGithub => {
                self.runtime.github_set_modal(true);
                self.runtime.github_connect();
            }
            // The window's own control requests, which it sends to the host after the draw, as the new-session
            // dialog's.
            Command::Start { agent, folder } => self.runtime.client_commands.push(start_request(agent, folder)),
            command => {
                debug_assert!(!command.is_view(), "the desktop does not show {command:?}");
                self.runtime.backend().send(command);
            }
        }
    }

    /// A reply to this window's own control request. Only its own starts select their result; an external start
    /// never passes through this handler.
    pub(super) fn control_reply(&mut self, start: bool, response: ControlResponse) {
        if let Some(error) = response.error {
            self.runtime.toast(error);
        } else if start && let Some(id) = response.result.get("id").and_then(|id| id.as_str()) {
            self.activate(id, false);
        }
    }

    /// Makes a session the active one; the diff panel follows it, and opens if `open_diff`. Not private:
    /// a call from another module would then find `State::activate` through `Deref` instead.
    pub(super) fn activate(&mut self, id: &str, open_diff: bool) {
        self.runtime.active = Some(id.to_string());
        self.runtime.diff_root = None;
        self.runtime.diff_open |= open_diff;
        if self.runtime.sessions.iter().any(|s| s.id == id && s.terminal.is_some()) {
            self.client.focus_terminal = Some(id.to_string());
            // The terminal takes the keyboard as the next frame draws it, and this may be on no input.
            self.client.ctx.request_repaint();
        }
        self.send(Command::SeeSession(id.to_string()));
    }

    #[cfg(test)]
    pub(super) fn pick_project(&mut self) {
        self.client.project_picker = Some(
            self.runtime.repos.first().and_then(|r| r.parent()).map(Path::to_path_buf).unwrap_or_else(paths::home),
        );
    }
}
