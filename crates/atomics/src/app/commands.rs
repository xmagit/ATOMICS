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
                state.send_review(&root);
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
            Command::OpenProject(root) => state.post(move |state| state.open_project(&root)),
            Command::CloseProject(root) => state.post(move |state| state.close_project(&root)),
            Command::OpenWorkspace(name) => state.post(move |state| state.open_workspace(&name)),
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
            Command::SelectTheme { name, host } => state.post(move |state| state.select_theme(&name, host)),
            Command::CheckSandbox => state.post(|state| state.check_sandbox()),
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
        self.client.ui.palette = self.palette();
        let mut terminals = Terminals { sessions: &self.runtime.sessions, views: &mut self.client.terminal_views };
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
            Command::PickProject => self.pick_project(),
            Command::HideChief => self.runtime.chief_open = false,
            Command::ResizeChief(width) => self.runtime.store.chief_width = width,
            Command::Toast(text) => self.runtime.toast(text),
            // The window's own control requests, which it sends to the host after the draw, as the new-session
            // dialog's.
            Command::Start { agent, folder } => self.runtime.client_commands.push(start_request(agent, folder)),
            command => {
                debug_assert!(!command.is_view(), "the desktop does not show {command:?}");
                self.runtime.backend().send(command);
            }
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
        }
        self.send(Command::SeeSession(id.to_string()));
    }

    pub(super) fn pick_project(&mut self) {
        self.client.project_picker = Some(
            self.runtime.repos.first().and_then(|r| r.parent()).map(Path::to_path_buf).unwrap_or_else(paths::home),
        );
    }
}
