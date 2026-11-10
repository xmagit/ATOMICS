//! The in-process [`Backend`]: reads the host model directly. The desktop's draw code reads through it,
//! so a loopback or browser client can read the same things over the protocol.
use atomics_ui::{
    ApprovalRow, Backend, BoardView, ChiefView, CompletedTile, DiffHeading, DiffSnapshot, Heartbeat, Notices, PlanRow,
    ProjectMenu, RecentSession, SandboxLoad, SessionRow, TileView, UiConfig, ViewHints, WorktreeRow,
};

use super::*;

/// A short-lived read of the model; build it with [`State::backend`]. Commands sent to it run as actions
/// (`commands.rs`).
pub(super) struct Local<'a>(pub(super) &'a State);

impl State {
    pub(super) fn session_title(&self, session: &Session) -> String {
        self.plans
            .iter()
            .flat_map(|plan| &plan.tasks)
            .find(|task| task.session.as_deref() == Some(&session.id))
            .map(|task| format!("{} · {}", task.id, task.title))
            .unwrap_or_else(|| session.title.clone())
    }

    /// Presentation state leaves the agent's hook lifecycle intact while explaining what it waits for.
    pub(super) fn session_status(&self, session: &Session) -> (AgentStatus, Option<String>) {
        if session.status != AgentStatus::Exited && session.approvals.is_empty() {
            let task = self
                .plans
                .iter()
                .flat_map(|plan| &plan.tasks)
                .find(|task| task.session.as_deref() == Some(&session.id));
            if self.pending_questions.contains(&session.id) && task.is_none_or(|task| task.status != TaskStatus::Done)
                || task.is_some_and(|task| task.status == TaskStatus::Blocked)
            {
                return (AgentStatus::NeedsInput, None);
            }
            if session.chief && session.status == AgentStatus::Working {
                let mut running = self
                    .traces
                    .get(&session.id)
                    .into_iter()
                    .flat_map(|log| &log.entries)
                    .filter(|entry| entry.state == atomics_core::trace::TraceState::Running)
                    .peekable();
                let only_waiting = running.peek().is_some() && running.all(|entry| entry.tool == "atomics_wait");
                let approval =
                    self.plans.iter().find(|plan| plan.is_open() && !plan.approved && !plan.tasks.is_empty());
                let active_work =
                    self.plans.iter().filter(|plan| plan.is_open()).flat_map(|plan| &plan.tasks).any(|task| {
                        matches!(task.status, TaskStatus::Running | TaskStatus::Review | TaskStatus::Blocked)
                    });
                if only_waiting
                    && !active_work
                    && let Some(plan) = approval
                {
                    return (AgentStatus::NeedsInput, Some(format!("waiting for you: approve {}", plan.goal)));
                }
            }
            if session.is_agent()
                && matches!(
                    session.status,
                    AgentStatus::Starting | AgentStatus::Idle | AgentStatus::Done | AgentStatus::NeedsInput
                )
                && let Some(terminal) = session.terminal.as_ref().filter(|terminal| !terminal.exited())
                && let Some(hint) = atomics_core::approvals::first_run_hint(&terminal.screen_text(60))
            {
                return (AgentStatus::NeedsInput, Some(hint.into()));
            }
        }
        (session.status, None)
    }

    pub(super) fn backend(&self) -> Local<'_> {
        Local(self)
    }
}

/// A finished task's worktree is still on disk: its `diff` and `resume` need it.
pub(super) fn worktree_kept(task: &Task) -> bool {
    task.finished_at.is_some() && task.worktree.as_ref().is_some_and(|path| path.is_dir())
}

/// The task runs a Codex agent (by its program, not its name), which resumes without a saved id.
pub(super) fn resumes_as_codex(config: &Config, task: &Task) -> bool {
    config.agent(&task.agent).is_some_and(|agent| agent.is_codex())
}

impl Local<'_> {
    fn row(&self, index: usize, plan: &Plan) -> PlanRow {
        PlanRow {
            id: plan.id.clone(),
            goal: plan.goal.clone(),
            tab_name: super::sidebar::plan_tab_name(plan),
            status: plan.status,
            state: self.0.plan_state(index),
            approved: plan.approved,
            changed: self.0.changed_plans.contains(&plan.id),
            done: plan.count(TaskStatus::Done),
            total: plan.tasks.len(),
            attention: plan.count(TaskStatus::Review) + plan.count(TaskStatus::Blocked),
        }
    }

    fn session(&self, session: &Session) -> SessionRow {
        let root = session.root();
        let (status, status_hint) = self.0.session_status(session);
        SessionRow {
            id: session.id.clone(),
            title: self.0.session_title(session),
            agent: session.agent.clone(),
            kind: session.kind,
            status,
            status_hint,
            chief: session.chief,
            worktree: session.worktree.as_ref().map(|worktree| worktree.root.clone()),
            branch: session.branch().map(str::to_string),
            project: self.0.project_of(&root),
            ungrouped: self.0.ungrouped_session(session),
            task: self.0.plans.iter().find_map(|plan| {
                let task = plan.tasks.iter().find(|task| task.session.as_deref() == Some(session.id.as_str()))?;
                Some(atomics_ui::TaskLink { plan: plan.id.clone(), task: task.id.clone() })
            }),
            scratch: session.scratch.is_some(),
            sandbox: session.sandbox.clone(),
            outside: session.terminal.is_none(),
            can_receive: session.can_receive(),
            pending_approvals: session.approvals.len(),
            held_messages: session.held.len(),
            changes: self
                .0
                .diffs
                .get(&root)
                .and_then(|data| data.prepared.as_ref())
                .map(|diff| (diff.additions, diff.deletions)),
            root,
        }
    }
}

impl Backend for Local<'_> {
    fn editors(&self) -> Vec<atomics_core::projection::EditorInfo> {
        self.0.editor_infos()
    }

    fn pull_requests(&self) -> Vec<atomics_core::projection::PullRequestInfo> {
        self.0.pull_request_infos()
    }
    fn send(&self, command: atomics_ui::Command) {
        self.run(command);
    }

    /// The desktop carries out every command. Its ticket reader needs a ticket system to read from.
    fn available(&self, command: &atomics_ui::Command) -> bool {
        if let atomics_ui::Command::CurateNote { plan, delete: false, .. } = command {
            return self.0.plans.get(*plan).is_some_and(|p| p.is_open()) && self.0.notebook_enabled(*plan);
        }
        !matches!(command, atomics_ui::Command::OpenTicket(_)) || self.0.ticket_config().is_some()
    }

    /// Only a project: from an app menu this is the home folder, which no agent should start in unasked.
    fn launch_dir(&self) -> Option<PathBuf> {
        self.0.launch_project.then(|| self.0.launch_dir.clone())
    }

    fn session_ticket(&self, session: &str) -> Option<atomics_core::tickets::TicketLink> {
        self.0.stamp_system(self.0.session_ticket(session))
    }

    fn ticket_tree(&self, plan: usize) -> Option<atomics_core::ticket_ui::TicketTree> {
        self.0.plans.get(plan).and_then(|plan| self.0.ticket_tree(plan))
    }

    fn tickets(&self) -> Option<atomics_core::ticket_ui::TicketsInfo> {
        self.0.ticket_ui_info()
    }

    fn tickets_panel(&self) -> Option<atomics_ui::TicketsPanel> {
        self.0.ticket_config().map(|_| atomics_ui::TicketsPanel { signed_out: self.0.tickets.panel.signin })
    }

    fn plan_list(&self) -> Vec<PlanRow> {
        self.0.plans.iter().enumerate().map(|(index, plan)| self.row(index, plan)).collect()
    }

    fn plan_row(&self, index: usize) -> Option<PlanRow> {
        self.0.plans.get(index).map(|plan| self.row(index, plan))
    }

    fn board(&self, index: usize) -> Option<BoardView> {
        let state = self.0;
        let plan = state.plans.get(index)?;
        let integration_notes = plan
            .tasks
            .iter()
            .filter_map(|task| {
                let note = match (state.agent_cleanup_note(task), state.integration_check_note(index, task)) {
                    (Some(cleanup), Some(integration)) => format!("{cleanup}; {integration}"),
                    (Some(note), None) | (None, Some(note)) => note,
                    (None, None) => return None,
                };
                let checking = state.integration_checks.contains_key(&(plan.id.clone(), task.id.clone()));
                Some((task.id.clone(), (note, checking)))
            })
            .collect();
        let board_counts = state.tickets.boards.get(&plan.id).map(|items| {
            let planned = |item: &atomics_core::tickets::TicketLink| {
                plan.tasks.iter().any(|task| task.ticket.as_ref().is_some_and(|link| link.same_ticket(item)))
            };
            (items.len(), items.iter().filter(|item| !planned(item)).count())
        });
        Some(BoardView {
            state: state.plan_state(index),
            ready: plan.ready().iter().map(|task| task.id.clone()).collect(),
            sessions: state
                .sessions
                .iter()
                .map(|s| (s.id.clone(), (state.session_title(s), state.session_status(s).0)))
                .collect(),
            integration_notes,
            checking: state.integration_checks.keys().any(|(id, _)| id == &plan.id),
            integrating: state
                .integrating
                .iter()
                .filter(|(id, _)| id == &plan.id)
                .map(|(_, task)| task.clone())
                .collect(),
            ticket_config: state.ticket_config(),
            board_counts,
            worktrees: plan.tasks.iter().filter(|task| worktree_kept(task)).map(|task| task.id.clone()).collect(),
            codex: plan
                .tasks
                .iter()
                .filter(|task| resumes_as_codex(&state.config, task))
                .map(|task| task.id.clone())
                .collect(),
            now: atomics_core::store::now(),
            plan: plan.clone(),
        })
    }

    fn plan_digest(&self, index: usize) -> Option<String> {
        self.0.plans.get(index).map(Plan::digest)
    }

    fn machine_available(&self) -> bool {
        true
    }
    fn machine(&self) -> Option<atomics_core::machine::Machine> {
        self.0.machine.snapshot.clone()
    }
    fn watch_machine(&self, open: bool) {
        if self.0.machine.desktop_open.swap(open, std::sync::atomic::Ordering::AcqRel) != open {
            self.0.wake.notify();
        }
    }
    fn previews(&self, session: Option<&str>) -> Vec<atomics_core::preview::Preview> {
        match session {
            Some(id) if self.0.sessions.iter().any(|s| s.id == id && s.status != AgentStatus::Exited) => {
                self.0.previews.list(id)
            }
            _ => Vec::new(),
        }
    }

    fn session_list(&self) -> Vec<SessionRow> {
        self.0.sessions.iter().map(|session| self.session(session)).collect()
    }

    fn session_row(&self, index: usize) -> Option<SessionRow> {
        self.0.sessions.get(index).map(|session| self.session(session))
    }

    fn session_approval(&self, index: usize) -> Option<ApprovalRow> {
        let session = self.0.sessions.get(index)?;
        let pending = session.approvals.first()?;
        Some(ApprovalRow { session: session.id.clone(), id: pending.id.clone(), request: pending.request.clone() })
    }

    fn tile(&self, index: usize) -> Option<TileView> {
        let state = self.0;
        let session = state.sessions.get(index)?;
        let reported = if matches!(session.status, AgentStatus::Idle | AgentStatus::Done) {
            state
                .plans
                .iter()
                .flat_map(|p| &p.tasks)
                .find(|t| t.session.as_deref() == Some(&session.id))
                .and_then(|t| t.reported_line(atomics_core::store::now()))
        } else {
            None
        };
        let queue_note = session.terminal.as_ref().and_then(|terminal| {
            let (running, waiting) = state.governor.snapshot();
            super::fleet::queue_note(terminal.process_id(), &running, &waiting)
        });
        let sandboxes = &state.sandbox_usage;
        Some(TileView {
            reported,
            sandbox_usage: session
                .sandbox
                .as_ref()
                .and_then(|name| sandboxes.usage.get(name))
                .map(|usage| usage.label()),
            sandbox_error: sandboxes.error.clone(),
            queue_note,
            warning: session.warning.clone(),
            connect: match session.sandbox {
                Some(_) => state.connect_buttons(&session.mcp_needed, &state.session_title(session)),
                None => Vec::new(),
            },
            exited: session.terminal.as_ref().is_some_and(|terminal| terminal.exited()),
            resumable: session.kind == AgentKind::Claude && session.claude_id.is_some(),
            folder: paths::display(&session.cwd),
            last_prompt: session.last_prompt.clone(),
            last_message: session.last_message.clone(),
        })
    }

    fn completed_tasks(&self) -> Vec<CompletedTile> {
        let state = self.0;
        let now = atomics_core::store::now();
        state
            .completed_tasks()
            .into_iter()
            .map(|(plan, index)| {
                let task = &state.plans[plan].tasks[index];
                CompletedTile {
                    plan,
                    index,
                    read_only: !state.plans[plan].is_open(),
                    worktree: worktree_kept(task),
                    codex: resumes_as_codex(&state.config, task),
                    now,
                    task: task.clone(),
                }
            })
            .collect()
    }

    fn diff_roots(&self) -> Vec<PathBuf> {
        self.0.diffs.keys().cloned().collect()
    }

    fn diff(&self, root: &Path) -> Option<DiffSnapshot> {
        if let Some((_, revision, prepared)) = self.0.pull_request_bodies().find(|(key, _, _)| Path::new(key) == root) {
            return Some(DiffSnapshot { revision, prepared: Some(prepared), ..DiffSnapshot::default() });
        }

        let data = self.0.diffs.get(root)?;
        Some(DiffSnapshot {
            revision: data.revision,
            prepared: data.prepared.clone(),
            error: data.error.clone(),
            loading: data.loading,
            comments: data.comments.clone(),
            viewing: data.viewing.clone(),
            file: data.file.clone(),
            file_error: data.file_error.clone(),
        })
    }

    fn trace_now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64
    }

    fn trace(&self) -> atomics_core::trace_feed::TraceData {
        self.0.trace_data()
    }

    fn chief(&self) -> ChiefView {
        let state = self.0;
        ChiefView {
            session: state.sessions.iter().position(|s| s.chief),
            open: state.chief_open,
            dock: state.chief_dock(),
            width: state.store.chief_width,
            heartbeat: state.heartbeat.last.as_ref().map(|(at, findings)| Heartbeat {
                age: atomics_core::store::now().saturating_sub(*at),
                time: local_time(*at),
                findings: findings.len(),
            }),
        }
    }

    fn diff_heading(&self, root: &Path) -> DiffHeading {
        let state = self.0;
        let session = state.active_index().map(|i| &state.sessions[i]);
        let title = session
            .map(|s| state.session_title(s))
            .unwrap_or_else(|| root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        let branch = session
            .and_then(|s| s.branch().map(str::to_string))
            .or_else(|| state.worktrees.iter().find(|w| w.path == root).and_then(|w| w.branch.clone()))
            .filter(|b| *b != title);
        let can_send = state.sessions.iter().any(|s| s.root() == root && s.is_agent() && s.can_receive());
        DiffHeading { title, branch, can_send }
    }

    fn ui_config(&self) -> UiConfig {
        let config = &self.0.config;
        UiConfig {
            keys: config.keys.clone(),
            font: config.font.clone(),
            layout: config.layout.clone(),
            agents: config.agents.iter().map(|agent| agent.name.clone()).collect(),
        }
    }

    fn view_hints(&self) -> ViewHints {
        let state = self.0;
        ViewHints {
            active: state.active.clone(),
            diff_target: state.diff_target(),
            plan_view: state.plan_view,
            ungrouped: state.ungrouped,
            project: state.view.clone(),
            selected_plan: state.selected_plan,
            diff_open: state.diff_open,
            panel: state.panel,
        }
    }

    fn visible_sessions(&self) -> Vec<usize> {
        self.0.visible_sessions()
    }

    fn checkouts(&self) -> Vec<String> {
        self.0.creating.iter().map(|(_, branch, _)| branch.clone()).collect()
    }

    fn recent_sessions(&self) -> Vec<RecentSession> {
        let state = self.0;
        let live_ids: HashSet<&str> = state.sessions.iter().filter_map(|s| s.claude_id.as_deref()).collect();
        state
            .store
            .sessions
            .iter()
            .filter(|r| !live_ids.contains(r.claude_session_id.as_str()) && r.cwd.is_dir() && r.agent != "chief")
            .filter(|r| state.project_of(&r.cwd).is_some())
            .filter({
                let mut seen = HashSet::new();
                move |r| seen.insert((r.title.clone(), r.cwd.clone()))
            })
            .take(8)
            .map(|record| RecentSession { folder: paths::display(&record.cwd), record: record.clone() })
            .collect()
    }

    fn repos(&self) -> Vec<PathBuf> {
        self.0.repos.clone()
    }

    fn project_of(&self, dir: &Path) -> Option<PathBuf> {
        self.0.project_of(dir)
    }

    fn repo_workflow(&self, project: &Path) -> Option<atomics_core::repo_workflow::RepoWorkflow> {
        self.0.repo_workflows.get(project).cloned()
    }

    fn worktrees(&self, project: &Path) -> Vec<WorktreeRow> {
        let state = self.0;
        state
            .worktrees
            .iter()
            .filter(|w| w.path != project && state.worktree_project.get(&w.path).map(PathBuf::as_path) == Some(project))
            .map(|w| WorktreeRow { path: w.path.clone(), branch: w.branch.clone() })
            .collect()
    }

    fn display(&self, path: &Path) -> String {
        paths::display(path)
    }

    fn plan_date(&self, index: usize) -> String {
        self.0.plans.get(index).map(super::plan_view::history_date).unwrap_or_default()
    }

    fn notices(&self) -> Notices {
        let state = self.0;
        Notices {
            toast: state.toast.as_ref().map(|(text, _)| text.clone()),
            config_error: state.config_error.clone(),
            config_problems: state.config_problems.clone(),
            theme_error: state.theme_error.clone(),
            hook_error: state.hook_error.clone(),
        }
    }

    fn sandbox_load(&self) -> SandboxLoad {
        let sampler = &self.0.sandbox_usage;
        SandboxLoad {
            total: (!sampler.usage.is_empty()).then(|| sampler.total().label()),
            sampled: sampler.usage.len(),
            error: sampler.error.clone(),
        }
    }

    fn management(&self) -> Option<atomics_core::projection::ManagementInfo> {
        Some(self.0.management_info())
    }

    fn update(&self) -> Option<atomics_core::projection::UpdateInfo> {
        Some(self.0.update_info())
    }

    fn project_menu(&self) -> ProjectMenu {
        let state = self.0;
        ProjectMenu {
            recent: state
                .store
                .recent_projects
                .iter()
                .filter(|p| !state.open_projects.contains(p) && p.is_dir())
                .take(8)
                .map(|p| (p.clone(), paths::display(p)))
                .collect(),
            workspaces: Workspace::list(),
            current: state.workspace.clone(),
            can_save: !state.open_projects.is_empty(),
        }
    }

    fn sign_in(&self) -> Option<atomics_ui::signin::SignInView> {
        self.0.sign_in_view()
    }

    fn resources(&self) -> Option<atomics_core::projection::ResourceInfo> {
        Some(self.0.resources.clone())
    }
}

/// `at`, in seconds since the epoch, as this machine's local date and time; "unavailable" if it cannot tell.
fn local_time(at: u64) -> String {
    let timestamp = at as libc::time_t;
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: both pointers are valid; a non-null result points to initialized local.
    let local = unsafe { libc::localtime_r(&timestamp, local.as_mut_ptr()).as_ref() };
    local.map_or_else(
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
    )
}

#[cfg(test)]
mod tests {
    use super::super::tests::{session, with_app};
    use super::*;

    #[test]
    fn commands_map_onto_atomics_v1() {
        with_app("app::backend::tests::commands_map_onto_atomics_v1", |app, _| {
            use atomics_core::ops::{
                self, ApprovalDecide, Outcome, PlanAction, PlanCommand, ReviewAction, ReviewCommand, SessionAction,
                SessionCommand,
            };
            use atomics_core::wire::Seq;
            use atomics_ui::Remote;
            let dir = crate::test_support::tempdir().unwrap();
            let root = dir.path().join("work");
            app.runtime.sessions = vec![session("agent", AgentKind::Claude, AgentStatus::Working, &root)];
            let done =
                |id: &str| Task { id: id.into(), status: TaskStatus::Done, branch: Some(id.into()), ..Task::default() };
            app.runtime.plans = vec![Plan {
                id: "plan".into(),
                goal: "Ship it".into(),
                tasks: vec![done("a"), done("b"), Task { id: "c".into(), ..Task::default() }],
                ..Plan::default()
            }];
            app.runtime.diffs.entry(root.clone()).or_default().revision = 7;
            let comment = |body: &str| steer::ReviewComment {
                path: "lib.rs".into(),
                line: Some(3),
                removed: false,
                excerpt: "fn x()".into(),
                body: body.into(),
            };
            let digest = app.runtime.plans[0].digest();
            let worktree = root.to_string_lossy().into_owned();
            let plan = |op: &str, action: PlanAction| {
                let digest = matches!(action, PlanAction::Approve).then(|| digest.clone());
                ops::Command::PlanCommand(PlanCommand { op: op.into(), plan: Some("plan".into()), digest, action })
            };
            let review = |op: &str, action: ReviewAction| {
                ops::Command::ReviewCommand(ReviewCommand { op: op.into(), worktree: worktree.clone(), action })
            };
            let session = |op: &str, action: SessionAction| {
                ops::Command::SessionCommand(SessionCommand { op: op.into(), action })
            };

            let backend = app.runtime.backend();
            let mut count = 0;
            let mut map = |command: Command| {
                command.remote(&backend, &mut || {
                    count += 1;
                    format!("op-{count}")
                })
            };
            assert_eq!(map(Command::ApprovePlan(0)), Remote::Send(vec![plan("op-1", PlanAction::Approve)]));
            // One integration per accepted task, each its own operation.
            assert_eq!(
                map(Command::IntegrateAccepted(0)),
                Remote::Send(vec![
                    plan("op-2", PlanAction::Integrate { task: "a".into() }),
                    plan("op-3", PlanAction::Integrate { task: "b".into() }),
                ])
            );
            let status = Command::SetTaskStatus { plan: 0, task: "c".into(), status: TaskStatus::Blocked };
            let update = PlanAction::Update { task: "c".into(), status: Some(TaskStatus::Blocked), note: None };
            assert_eq!(map(status), Remote::Send(vec![plan("op-4", update)]));
            assert_eq!(map(Command::ApprovePlan(1)), Remote::Send(Vec::new()));
            let decide = Command::Decide { session: 0, approval: "ask".into(), decision: Decision::Deny };
            let answer =
                ApprovalDecide { op: "op-5".into(), approval: "ask".into(), decision: Decision::Deny, message: None };
            assert_eq!(map(decide), Remote::Send(vec![ops::Command::ApprovalDecide(answer)]));
            // An edited comment is removed and added again, anchored to the diff's revision.
            let edit = Command::EditComments {
                root: root.clone(),
                before: vec![comment("keep"), comment("old")],
                after: vec![comment("keep"), comment("new")],
            };
            let (path, line, removed) = (String::from("lib.rs"), Some(3), false);
            assert_eq!(
                map(edit),
                Remote::Send(vec![
                    review("op-6", ReviewAction::Remove { path: path.clone(), line, removed, text: "old".into() }),
                    review("op-7", ReviewAction::Add { revision: Seq(7), path, line, removed, text: "new".into() }),
                ])
            );
            assert_eq!(map(Command::SendReview(root.clone())), Remote::Send(vec![review("op-8", ReviewAction::Send)]));
            let steer = SessionAction::Steer { session: "agent".into(), text: "go".into() };
            assert_eq!(
                map(Command::VoiceToSession { session: 0, text: "go".into() }),
                Remote::Send(vec![session("op-9", steer)])
            );
            let close = SessionAction::Close { session: "agent".into() };
            assert_eq!(map(Command::CloseSession("agent".into())), Remote::Send(vec![session("op-10", close)]));
            assert_eq!(map(Command::ToggleDiff), Remote::Client);
            assert_eq!(map(Command::ShowPlan(0)), Remote::Client);
            assert_eq!(
                map(Command::OpenProject(root.clone())),
                Remote::Send(vec![ops::Command::HostCommand(ops::HostCommand {
                    op: "op-11".into(),
                    action: ops::HostAction::OpenProject { path: worktree.clone() }
                })])
            );
            assert_eq!(map(Command::OpenTicket(Default::default())), Remote::Client);
            assert_eq!(map(Command::SyncTickets(0)), Remote::Unsupported);
            assert_eq!(
                map(Command::ClosePlan { plan: 0, status: PlanStatus::Completed }),
                Remote::Send(vec![plan("op-12", PlanAction::Complete)])
            );

            // The host takes what the mapping sends: approving through the journal, as a protocol client does.
            let Remote::Send(mut sent) = Command::ApprovePlan(0).remote(&app.runtime.backend(), &mut String::new)
            else {
                unreachable!()
            };
            let Some(ops::Command::PlanCommand(approve)) = sent.pop() else { unreachable!() };
            let outcome = app.runtime.mutate_desktop(|op| ops::Command::PlanCommand(PlanCommand { op, ..approve }));
            assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
            assert!(app.runtime.plans[0].approved);
        });
    }

    #[test]
    fn session_rows_follow_the_model() {
        with_app("app::backend::tests::session_rows_follow_the_model", |app, _| {
            let dir = crate::test_support::tempdir().unwrap();
            let (repo, worktree, scratch) = (dir.path().join("repo"), dir.path().join("task"), dir.path().join("note"));
            app.runtime.repos = vec![repo.clone()];
            app.runtime.worktree_project.insert(worktree.clone(), repo.clone());
            let mut planned = session("planned", AgentKind::Claude, AgentStatus::Working, &worktree);
            planned.worktree = Some(Worktree { root: worktree.clone(), branch: Some("task".into()) });
            let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &repo);
            chief.chief = true;
            let mut loose = session("loose", AgentKind::Generic, AgentStatus::NeedsInput, &scratch);
            loose.scratch = Some(scratch.clone());
            loose.sandbox = Some("vm".into());
            loose.held.push("after the decision".into());
            app.runtime.sessions = vec![planned, chief, loose];
            app.runtime.plans = vec![Plan {
                id: "plan".into(),
                tasks: vec![Task {
                    id: "t1".into(),
                    title: "Fix the parser".into(),
                    session: Some("planned".into()),
                    ..Task::default()
                }],
                ..Plan::default()
            }];
            app.runtime.diffs.entry(worktree.clone()).or_default().prepared = Some(Arc::new(diff_view::Prepared {
                diff: Default::default(),
                highlights: vec![],
                longest_line: 0,
                additions: 12,
                deletions: 3,
                all_files: Arc::default(),
            }));

            let backend = app.runtime.backend();
            let rows = backend.session_list();
            assert_eq!(rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), ["planned", "chief", "loose"]);
            assert_eq!(backend.session_row(2).as_ref(), Some(&rows[2]));
            assert_eq!(backend.session_row(3), None);
            let [planned, chief, loose] = &rows[..] else { unreachable!() };
            assert_eq!(planned.root, worktree);
            assert_eq!(planned.worktree.as_ref(), Some(&worktree));
            assert_eq!(planned.branch.as_deref(), Some("task"));
            assert_eq!(planned.title, "t1 · Fix the parser");
            assert_eq!(app.runtime.describe(0)["title"], "t1 · Fix the parser");
            assert_eq!(app.runtime.describe(0)["branch"], "task");
            assert_eq!(loose.title, "loose");
            assert_eq!(planned.project.as_ref(), Some(&repo));
            assert_eq!(planned.changes, Some((12, 3)));
            assert!(!planned.ungrouped);
            assert!(chief.chief && !chief.ungrouped && chief.project.as_ref() == Some(&repo));
            assert_eq!((chief.root.as_path(), chief.worktree.as_ref(), chief.changes), (repo.as_path(), None, None));
            assert!(loose.ungrouped && loose.scratch && loose.project.is_none());
            assert_eq!(
                (loose.kind, loose.status, loose.agent.as_str()),
                (AgentKind::Generic, AgentStatus::NeedsInput, "test")
            );
            assert_eq!((loose.sandbox.as_deref(), loose.pending_approvals, loose.held_messages), (Some("vm"), 0, 1));
            assert!(rows.iter().all(|row| row.outside), "the test's sessions have no terminal in ATOMICS");
        });
    }

    #[test]
    fn the_chief_and_who_can_take_a_steer_follow_the_model() {
        with_app("app::backend::tests::the_chief_and_who_can_take_a_steer_follow_the_model", |app, _| {
            let dir = crate::test_support::tempdir().unwrap();
            let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, dir.path());
            chief.chief = true;
            chief.claude_id = Some("conversation".into());
            let mut gone = session("gone", AgentKind::Claude, AgentStatus::Exited, dir.path());
            gone.claude_id = Some("old".into());
            app.runtime.sessions = vec![gone, chief];
            app.runtime.chief_open = true;
            app.runtime.store.chief_dock = Some(ChiefDock::Docked);
            app.runtime.store.chief_width = Some(640.0);
            app.runtime.heartbeat.last = Some((atomics_core::store::now() - 120, Vec::new()));

            let backend = app.runtime.backend();
            let chief = backend.chief();
            assert_eq!(chief.session, Some(1));
            assert_eq!((chief.open, chief.dock, chief.width), (true, ChiefDock::Docked, Some(640.0)));
            let heartbeat = chief.heartbeat.expect("the heartbeat ran");
            assert!((120..180).contains(&heartbeat.age), "{}", heartbeat.age);
            assert_eq!(heartbeat.findings, 0);
            assert_eq!(heartbeat.time.len(), "2026-11-03 12:00:00".len(), "{}", heartbeat.time);
            let rows = backend.session_list();
            assert_eq!(rows.iter().map(|row| row.can_receive).collect::<Vec<_>>(), [false, true]);
        });
    }

    #[test]
    fn boards_and_approvals_follow_the_model() {
        with_app("app::backend::tests::boards_and_approvals_follow_the_model", |app, _| {
            use atomics_core::tickets::{TicketConfig, TicketLink};
            let dir = crate::test_support::tempdir().unwrap();
            let ticket = |id: &str| TicketLink { system: "relay".into(), id: id.into(), ..TicketLink::default() };
            let pending = |id: &str, command: &str| PendingApproval {
                id: id.into(),
                request: ApprovalRequest {
                    tool: "Bash".into(),
                    command: Some(command.into()),
                    cwd: dir.path().into(),
                    reason: None,
                    input: json!({}),
                    warning: None,
                },
                reply: None,
                native: None,
                stash: Default::default(),
            };
            let mut asking = session("asking", AgentKind::Claude, AgentStatus::NeedsInput, dir.path());
            asking.title = "Asking".into();
            asking.approvals = vec![pending("first", "touch first"), pending("second", "touch second")];
            let quiet = session("quiet", AgentKind::Claude, AgentStatus::Working, dir.path());
            app.runtime.sessions = vec![asking, quiet];
            let task = |id: &str, status: TaskStatus| Task {
                id: id.into(),
                title: format!("Task {id}"),
                status,
                ..Task::default()
            };
            app.runtime.plans = vec![
                Plan { id: "other".into(), ..Plan::default() },
                Plan {
                    id: "plan".into(),
                    tasks: vec![
                        Task {
                            session: Some("asking".into()),
                            ticket: Some(ticket("T-1")),
                            ..task("a", TaskStatus::Done)
                        },
                        Task { depends_on: vec!["a".into()], ..task("b", TaskStatus::Planned) },
                        Task { depends_on: vec!["c".into()], ..task("blocked", TaskStatus::Planned) },
                        task("c", TaskStatus::Running),
                    ],
                    ..Plan::default()
                },
            ];
            let config = Arc::new(TicketConfig { system: "relay".into(), ..TicketConfig::default() });
            app.runtime.tickets.config = Some(config.clone());
            app.runtime.tickets.boards.insert("plan".into(), vec![ticket("T-1"), ticket("T-2"), ticket("T-3")]);
            app.runtime.integration_checks.insert(("plan".into(), "a".into()), (0, Instant::now()));
            app.runtime.integrating.insert(("plan".into(), "a".into()));
            app.runtime.integrating.insert(("other".into(), "c".into()));

            let backend = app.runtime.backend();
            let board = backend.board(1).unwrap();
            assert_eq!(board.plan, app.runtime.plans[1]);
            assert_eq!(board.state, app.runtime.plan_state(1));
            assert_eq!(board.ready, ["b"]);
            assert_eq!(board.sessions["asking"], ("a · Task a".to_string(), AgentStatus::NeedsInput));
            assert_eq!(board.sessions.len(), 2);
            let note = format!(
                "Agent kept open: waiting for its turn and pending messages to finish; checking whether it is in {}",
                app.runtime.plans[1].integration_branch()
            );
            assert_eq!(board.integration_notes.len(), 1);
            assert_eq!(board.integration_notes["a"], (note, true));
            assert!(board.checking);
            assert_eq!(board.integrating, HashSet::from(["a".to_string()]));
            assert_eq!(board.ticket_config, Some(config));
            assert_eq!(board.board_counts, Some((3, 2)));
            let other = backend.board(0).unwrap();
            assert!(!other.checking && other.integration_notes.is_empty());
            assert_eq!((other.integrating, other.board_counts), (HashSet::from(["c".to_string()]), None));
            assert_eq!(backend.board(2), None);

            let approval = backend.session_approval(0).unwrap();
            assert_eq!((approval.session.as_str(), approval.id.as_str()), ("asking", "first"));
            assert_eq!(approval.request, app.runtime.sessions[0].approvals[0].request);
            assert_eq!(backend.session_approval(1), None);
            assert_eq!(backend.session_approval(2), None);
        });
    }
}
