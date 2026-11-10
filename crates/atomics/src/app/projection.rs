//! Builds the protocol's projections from the native model. Runs on the runtime thread, under
//! the `State` lock, with plain data only: it does no I/O and never blocks on a client. Readers
//! load the published `Arc` snapshots and never touch `State`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use atomics_core::diff::FileDiff;
use atomics_core::diff_data::Prepared;

use atomics_core::projection::{
    ApprovalInfo, DiffSummary, HostInfo, PlanInfo, Projections, ReportInfo, SessionInfo, TaskInfo, TaskLink,
};

use super::State;
use crate::protocol::{Departed, TerminalEntry, Terminals};

/// Diff summaries already built, by worktree and the diff revision they were built from, so an
/// unchanged diff is not walked again on every runtime pass.
#[derive(Default)]
pub(crate) struct DiffSummaries(HashMap<PathBuf, (u64, DiffSummary)>);

/// The accepted diff content per worktree, published so the protocol thread can answer `diff_get`
/// without the `State` lock. Holds `Arc`s of what the diff worker already built, and remembers
/// worktrees whose diff went away lately.
#[derive(Default)]
pub(crate) struct DiffBodies {
    contents: RwLock<HashMap<String, (u64, Arc<Prepared>)>>,
    departed: Departed,
    folders: Mutex<HashMap<String, usize>>,
    wake: crate::host::Wake,
}

impl DiffBodies {
    pub(crate) fn with_wake(wake: crate::host::Wake) -> Self {
        Self { wake, ..Default::default() }
    }

    /// A remote explorer can open an otherwise idle worktree. Its subscription wakes the host,
    /// which schedules the existing worker and keeps that file tree fresh only while followed.
    pub(crate) fn refresh_folders(&self, state: &mut State) {
        let roots: Vec<_> = self.folders.lock().unwrap_or_else(|e| e.into_inner()).keys().map(PathBuf::from).collect();
        for root in roots {
            if !state.repos.contains(&root)
                && !state.worktree_project.contains_key(&root)
                && !state.sessions.iter().any(|s| s.root() == root)
            {
                continue;
            }
            let diff = state.diffs.entry(root).or_default();
            if !diff.loading
                && diff.last_request.is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(3))
                && diff.dirty_since.is_none()
            {
                diff.mark_dirty();
            }
        }
    }

    fn replace(&self, entries: HashMap<String, (u64, Arc<Prepared>)>) {
        let same = {
            let current = self.contents.read().unwrap_or_else(|e| e.into_inner());
            current.len() == entries.len()
                && entries
                    .iter()
                    .all(|(k, (r, p))| current.get(k).is_some_and(|(cr, cp)| cr == r && Arc::ptr_eq(cp, p)))
        };
        if !same {
            let before = std::mem::replace(&mut *self.contents.write().unwrap_or_else(|e| e.into_inner()), entries);
            let now = self.contents.read().unwrap_or_else(|e| e.into_inner());
            self.departed.record(before.keys().map(String::as_str), |w| now.contains_key(w));
        }
    }
}

impl crate::protocol::DiffSource for DiffBodies {
    fn watch_folder(&self, worktree: &str, watching: bool) {
        let mut folders = self.folders.lock().unwrap_or_else(|e| e.into_inner());
        if watching {
            *folders.entry(worktree.into()).or_default() += 1;
        } else if let Some(count) = folders.get_mut(worktree) {
            *count -= 1;
            if *count == 0 {
                folders.remove(worktree);
            }
        }
        drop(folders);
        self.wake.notify();
    }
    fn folder_revision(&self, worktree: &str) -> Option<u64> {
        self.contents.read().unwrap_or_else(|e| e.into_inner()).get(worktree).map(|(revision, _)| *revision)
    }

    fn folder(
        &self,
        worktree: &str,
        path: &str,
        offset: u32,
        budget: usize,
    ) -> Result<atomics_core::folder::Page, atomics_core::wire::WireError> {
        let (revision, prepared) =
            self.contents.read().unwrap_or_else(|e| e.into_inner()).get(worktree).cloned().ok_or_else(|| {
                atomics_core::wire::WireError::new(atomics_core::wire::ErrorCode::NotFound, "file listing is not ready")
            })?;
        atomics_core::folder::page(
            prepared.all_files.iter().map(|p| p.as_ref()).chain(prepared.diff.files.iter().map(|f| f.path())),
            worktree,
            path,
            revision,
            offset,
            budget,
        )
    }

    fn content(
        &self,
        worktree: &str,
        path: &str,
        budget: usize,
    ) -> Result<(String, bool), atomics_core::wire::WireError> {
        use atomics_core::wire::{ErrorCode, WireError};
        let missing = || WireError::new(ErrorCode::NotFound, "no such worktree file");
        if path.is_empty() || !atomics_core::folder::valid_path(path) {
            return Err(missing());
        }
        let known =
            self.contents.read().unwrap_or_else(|e| e.into_inner()).get(worktree).is_some_and(|(_, prepared)| {
                prepared.all_files.iter().any(|p| p.as_ref() == path)
                    || prepared.diff.files.iter().any(|f| f.path() == path)
            });
        if !known {
            return Err(missing());
        }
        let (bytes, truncated) =
            match atomics_core::git::read_worktree_file(std::path::Path::new(worktree), path, budget as u64) {
                Some(atomics_core::git::WorktreeFile::File { bytes: Some(bytes), .. }) => (bytes, false),
                Some(atomics_core::git::WorktreeFile::File { bytes: None, .. }) => (Vec::new(), true),
                Some(atomics_core::git::WorktreeFile::Link(_)) => {
                    return Err(WireError::new(ErrorCode::Rejected, "symbolic link; file contents are not followed"));
                }
                None => return Err(missing()),
            };
        if bytes.contains(&0) {
            return Err(WireError::new(ErrorCode::Rejected, "binary file"));
        }
        let content = String::from_utf8_lossy(&bytes).into_owned();
        Ok((content, truncated))
    }

    fn file(&self, worktree: &str, path: &str) -> Option<(u64, FileDiff)> {
        let (revision, prepared) = self.contents.read().unwrap_or_else(|e| e.into_inner()).get(worktree)?.clone();
        let file = prepared
            .diff
            .files
            .iter()
            .find(|f| f.new_path.as_deref() == Some(path) || f.old_path.as_deref() == Some(path))?;
        Some((revision, file.clone()))
    }

    fn moved_on(&self, worktree: &str, revision: u64) -> bool {
        match self.contents.read().unwrap_or_else(|e| e.into_inner()).get(worktree) {
            Some((current, _)) => *current != revision,
            None => self.departed.contains(worktree),
        }
    }
}

fn text(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

impl State {
    pub(super) fn editor_infos(&self) -> Vec<atomics_core::projection::EditorInfo> {
        let mut editors: Vec<_> = self
            .editors
            .iter()
            .map(|(root, terminal)| {
                let path = text(root);
                atomics_core::projection::EditorInfo {
                    terminal: atomics_core::projection::editor_terminal(&path),
                    root: path,
                    running: !terminal.exited(),
                    file: self.editor_files.get(root).cloned(),
                    inlay_hints: !self.config.editor.inlay_hints_keys.is_empty(),
                }
            })
            .collect();
        editors.sort_by(|a, b| a.root.cmp(&b.root));
        editors
    }
    /// Publish which terminals exist, by session, for the protocol thread to stream. Called only
    /// while the protocol server runs; builds handles only when the set of terminals changed.
    pub(crate) fn publish_terminals(&self, out: &Terminals) {
        let live: Vec<_> = self
            .sessions
            .iter()
            .filter_map(|s| Some((s.id.clone(), s.terminal.as_ref()?)))
            .chain(
                self.editors
                    .iter()
                    .filter(|(_, t)| !t.exited())
                    .map(|(root, t)| (atomics_core::projection::editor_terminal(&text(root)), t)),
            )
            .collect();
        if out.matches(live.iter().map(|(id, t)| (id.as_str(), t.screen_identity()))) {
            return;
        }
        out.publish(
            live.into_iter()
                .map(|(id, t)| {
                    let handle = Arc::new(t.screen_handle());
                    TerminalEntry {
                        session: id,
                        identity: t.screen_identity(),
                        source: handle.clone(),
                        control: handle,
                    }
                })
                .collect(),
        );
    }

    /// The `[remote]` settings, for the host to decide whether to start the protocol server.
    pub(crate) fn remote_access(&self) -> atomics_core::config::RemoteAccessConfig {
        self.config.remote.clone()
    }

    pub(crate) fn set_remote_view(&mut self, view: crate::host::RemoteView) {
        self.remote = view;
    }

    pub(crate) fn remote_view(&self) -> &crate::host::RemoteView {
        &self.remote
    }

    /// The ticket profile in effect, which the rendezvous registers and buys passes through.
    pub(crate) fn rendezvous_profile(&self) -> Option<std::sync::Arc<atomics_core::tickets::TicketConfig>> {
        self.ticket_config()
    }

    /// The rendezvous's status, when `[remote.rendezvous]` is enabled; read under its own short lock.
    pub(crate) fn rendezvous_status(&self) -> Option<crate::protocol::rendezvous::Status> {
        self.remote.rendezvous.as_ref().map(|cell| cell.get())
    }

    /// The paired devices and the pairing, while the rendezvous runs.
    pub(crate) fn remote_devices(&self) -> Option<std::sync::Arc<crate::protocol::rendezvous::devices::Devices>> {
        self.remote.devices.clone()
    }

    /// Why clients' mutations are refused until restart, when the operation journal could not be
    /// opened or written ([`crate::protocol::journal::Journal::error`]). The desktop's own actions
    /// still work.
    pub(crate) fn journal_problem(&self) -> Option<String> {
        self.ops.error().map(|error| {
            format!(
                "The operation journal cannot be written ({error}), so phones, browsers and other clients cannot \
                 change anything until ATOMICS restarts."
            )
        })
    }

    /// `atomics ctl '{"cmd":"remote"}'`: the protocol server (with `journal_error` when the operation
    /// journal cannot be written), the rendezvous and the paired devices. Never a token, pass, psk or
    /// pairing link.
    pub(crate) fn remote_json(&self) -> serde_json::Value {
        use crate::protocol::rendezvous::devices::PairingView;
        let view = &self.remote;
        let devices = view.devices.as_ref().map(|devices| {
            let pairing = match devices.view() {
                PairingView::Idle { .. } => "none",
                PairingView::Open { .. } => "open",
                PairingView::Confirm { .. } => "confirm",
            };
            let list: Vec<_> = devices
                .list()
                .into_iter()
                .map(|(device, connected)| {
                    serde_json::json!({
                        "name": device.name,
                        "access": device.access,
                        "paired_ms": device.paired_ms,
                        "seen_ms": device.seen_ms,
                        "connected": connected,
                    })
                })
                .collect();
            serde_json::json!({ "pairing": pairing, "devices": list, "problem": devices.problem() })
        });
        serde_json::json!({
            "protocol": {
                "enabled": view.enabled,
                "url": view.addr.map(|addr| format!("ws://{addr}")),
                "error": view.error,
                "journal_error": self.ops.error(),
            },
            "rendezvous": self.rendezvous_status(),
            "paired": devices,
            "clients": view.clients.as_ref().map(|clients| {
                clients.list().into_iter().map(|c| serde_json::json!({ "name": c.name, "paired_at": c.paired_at })).collect::<Vec<_>>()
            }),
        })
    }

    /// `pair_client`: a token for a client that reaches the protocol server through a forward. Only
    /// while the server runs; the token is in this answer and nowhere else.
    pub(crate) fn pair_client(&self, name: &str) -> atomics_core::control::ControlResponse {
        use atomics_core::control::ControlResponse;
        let Some(clients) = &self.remote.clients else {
            return ControlResponse::error(
                "the protocol server is off: enable [remote] or start atomics host --remote-port",
            );
        };
        match clients.pair(name) {
            Ok((name, token)) => ControlResponse::ok(serde_json::json!({ "name": name, "token": token })),
            Err(error) => ControlResponse::error(format!("pairing failed: {error}")),
        }
    }

    pub(crate) fn unpair_client(&self, name: &str) -> atomics_core::control::ControlResponse {
        use atomics_core::control::ControlResponse;
        let Some(clients) = &self.remote.clients else {
            return ControlResponse::error("the protocol server is off");
        };
        match clients.unpair(name) {
            Ok(true) => ControlResponse::ok(serde_json::json!({ "name": name.trim() })),
            Ok(false) => ControlResponse::error(format!("no client is paired as {name}")),
            Err(error) => ControlResponse::error(format!("unpairing failed: {error}")),
        }
    }

    /// Publish sessions, plans, approvals and diff summaries. Every pass rebuilds them; each
    /// collection's revision, and so the event cursor, moves only when its content does.
    pub(crate) fn publish_projections(&self, out: &Projections, cache: &mut DiffSummaries, bodies: &DiffBodies) {
        out.publish_sessions(self.session_infos());
        out.publish_plans(self.plan_infos());
        // How answered approvals ended goes first, so the event for one leaving says how.
        out.publish_resolutions(&self.resolved);
        out.publish_approvals(self.approval_infos());
        self.publish_diffs(out, cache, bodies);
        out.publish_host(self.host_info());
        out.trace.publish(self.trace_data());
        if let Some(snapshot) = &self.machine.snapshot {
            out.machine.publish(snapshot.clone());
        }
    }

    /// The host's own state: the open workspace, the saved ones and the ticket system's sign-in.
    pub(crate) fn host_info(&self) -> HostInfo {
        HostInfo {
            editors: self.editor_infos(),
            pull_requests: self.pull_request_infos(),
            projects: self
                .repos
                .iter()
                .map(|root| atomics_core::projection::ProjectInfo {
                    root: text(root),
                    worktrees: self
                        .worktrees
                        .iter()
                        .filter(|w| w.path != *root && self.worktree_project.get(&w.path) == Some(root))
                        .map(|w| atomics_core::projection::WorktreeInfo {
                            path: text(&w.path),
                            branch: w.branch.clone(),
                        })
                        .collect(),
                })
                .collect(),
            workspace: self.workspace.clone(),
            workspaces: self.workspace_names.clone(),
            sign_in: self.sign_in_info(),
            tickets: self.ticket_ui_info(),
            management: Some(self.management_info()),
            resources: Some(self.resources.clone()),
            update: Some(self.update_info()),
        }
    }

    fn session_infos(&self) -> Vec<SessionInfo> {
        // Each session's task, found in one walk over the plans rather than one per session: the
        // first plan, in order, with a task on the session, and that plan's first such task.
        let mut links: HashMap<&str, TaskLink> = HashMap::new();
        for plan in &self.plans {
            for task in &plan.tasks {
                if let Some(session) = task.session.as_deref() {
                    links.entry(session).or_insert_with(|| TaskLink { plan: plan.id.clone(), task: task.id.clone() });
                }
            }
        }
        let task_of = |session: &str| links.get(session).cloned();
        self.sessions
            .iter()
            .map(|s| SessionInfo {
                previews: if s.status == atomics_core::session::AgentStatus::Exited {
                    Vec::new()
                } else {
                    self.previews.list(&s.id)
                },
                id: s.id.clone(),
                agent: s.agent.clone(),
                title: s.title.clone(),
                cwd: text(&s.cwd),
                worktree: s.worktree.as_ref().map(|w| text(&w.root)),
                branch: s.branch().map(str::to_string),
                status: s.status,
                chief: s.chief,
                task: task_of(&s.id),
                pending_approvals: s.approvals.len() as u32,
            })
            .collect()
    }

    fn plan_infos(&self) -> Vec<PlanInfo> {
        self.plans
            .iter()
            .map(|p| PlanInfo {
                notebook: p.notebook.clone(),
                ticket_tree: self.ticket_tree(p),
                id: p.id.clone(),
                status: p.status,
                goal: p.goal.clone(),
                project: p.project.as_deref().map(text),
                approved: p.approved,
                integration: p.integration.clone(),
                tasks: p
                    .tasks
                    .iter()
                    .map(|t| TaskInfo {
                        id: t.id.clone(),
                        title: t.title.clone(),
                        status: t.status,
                        agent: t.agent.clone(),
                        depends_on: t.depends_on.clone(),
                        session: t.session.clone(),
                        branch: t.branch.clone(),
                        integrated: t.integrated.clone(),
                        additions: t.additions,
                        deletions: t.deletions,
                        report: t.report.as_ref().map(|r| ReportInfo {
                            status: r.status.clone(),
                            summary: r.summary.clone(),
                            at: r.at,
                        }),
                        ticket: t.ticket.as_ref().map(|ticket| ticket.label().to_string()),
                    })
                    .collect(),
                // Only remote clients need the approval scope, and only open plans can be approved or replaced.
                digest: if self.config.remote.enabled && p.status == atomics_core::plan::PlanStatus::Open {
                    p.digest()
                } else {
                    String::new()
                },
                ticket: p.ticket.as_ref().map(|ticket| ticket.label().to_string()),
            })
            .collect()
    }

    pub(super) fn approval_infos(&self) -> Vec<ApprovalInfo> {
        self.sessions
            .iter()
            .flat_map(|s| {
                s.approvals.iter().map(|a| ApprovalInfo {
                    id: a.id.clone(),
                    session: s.id.clone(),
                    tool: a.request.tool.clone(),
                    command: a.request.command.clone(),
                    cwd: text(&a.request.cwd),
                    reason: a.request.reason.clone(),
                    warning: a.request.warning.clone(),
                })
            })
            .collect()
    }

    fn publish_diffs(&self, out: &Projections, cache: &mut DiffSummaries, bodies: &DiffBodies) {
        let pull_bodies: Vec<_> = self.pull_request_bodies().collect();
        cache.0.retain(|root, _| {
            self.diffs.contains_key(root) || pull_bodies.iter().any(|(key, _, _)| std::path::Path::new(key) == root)
        });
        let mut diffs = Vec::new();
        let mut contents = HashMap::new();
        for (root, data) in &self.diffs {
            let Some(prepared) = &data.prepared else { continue };
            contents.insert(text(root), (data.revision, prepared.clone()));
            let entry = match cache.0.get(root) {
                Some((revision, summary)) if *revision == data.revision => summary.clone(),
                _ => {
                    let summary = DiffSummary::new(&text(root), data.revision, &prepared.diff);
                    cache.0.insert(root.clone(), (data.revision, summary.clone()));
                    summary
                }
            };
            diffs.push(DiffSummary { comments: data.comments.clone(), ..entry });
        }
        for (key, revision, prepared) in pull_bodies {
            let root = PathBuf::from(&key);
            let summary = match cache.0.get(&root) {
                Some((r, summary)) if *r == revision => summary.clone(),
                _ => {
                    let summary = DiffSummary::new(&key, revision, &prepared.diff);
                    cache.0.insert(root, (revision, summary.clone()));
                    summary
                }
            };
            diffs.push(summary);
            contents.insert(key, (revision, prepared));
        }
        diffs.sort_by(|a, b| a.worktree.cmp(&b.worktree));
        // Bodies first: a client told about a revision can always fetch it.
        bodies.replace(contents);
        out.publish_diffs(diffs);
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{session, with_state};
    use super::super::{
        AgentKind, AgentStatus, ApprovalRequest, PendingApproval, Plan, PlanStatus, Report, State, Task, TaskStatus,
    };
    use super::{Arc, DiffBodies, DiffSummaries, Prepared, Projections};
    use crate::protocol::DiffSource;
    use atomics_core::diff::{FileDiff, FileStatus, WorktreeDiff};
    use atomics_core::projection::{ApprovalInfo, PlanInfo, SessionInfo};

    fn approval(id: &str) -> PendingApproval {
        PendingApproval {
            id: id.into(),
            request: ApprovalRequest {
                tool: "Bash".into(),
                command: Some("git status".into()),
                cwd: "/w".into(),
                reason: None,
                input: serde_json::json!({}),
                warning: None,
            },
            reply: None,
            native: None,
            stash: Default::default(),
        }
    }

    fn task(id: &str, session: Option<&str>) -> Task {
        Task { id: id.into(), title: format!("task {id}"), session: session.map(str::to_string), ..Task::default() }
    }

    fn prepared(files: usize) -> Arc<Prepared> {
        let file = |n: usize| FileDiff {
            old_path: None,
            new_path: Some(format!("src/file{n}.rs")),
            status: FileStatus::Added,
            binary: false,
            hunks: vec![],
            omitted: 0,
        };
        Arc::new(Prepared {
            diff: WorktreeDiff { files: (0..files).map(file).collect(), ..WorktreeDiff::default() },
            highlights: vec![],
            longest_line: 0,
            additions: 0,
            deletions: 0,
            all_files: Arc::default(),
        })
    }

    /// Everything a client would be told, as it stands in `out`.
    fn published(out: &Projections) -> (Vec<SessionInfo>, Vec<PlanInfo>, Vec<ApprovalInfo>) {
        let l = out.load();
        (l.sessions.data.clone(), l.plans.data.clone(), l.approvals.data.clone())
    }

    fn built(state: &State) -> (Vec<SessionInfo>, Vec<PlanInfo>, Vec<ApprovalInfo>) {
        (state.session_infos(), state.plan_infos(), state.approval_infos())
    }

    type Change = Box<dyn Fn(&mut State)>;

    fn change(f: impl Fn(&mut State) + 'static) -> Change {
        Box::new(f)
    }

    /// The event cursor and each collection's revision, as they stand in `out`.
    fn revisions(out: &Projections) -> (u64, [u64; 4]) {
        let l = out.load();
        (l.cursor, [l.sessions.revision, l.plans.revision, l.approvals.revision, l.diffs.revision])
    }

    #[test]
    fn plan_digests_are_published_only_with_remote_on() {
        with_state("app::projection::tests::plan_digests_are_published_only_with_remote_on", |mut state, _| {
            state.plans = vec![Plan::default()];
            assert!(state.plan_infos()[0].digest.is_empty());
            state.config.remote.enabled = true;
            assert_eq!(state.plan_infos()[0].digest, state.plans[0].digest());
            state.plans[0].status = atomics_core::plan::PlanStatus::Completed;
            assert!(state.plan_infos()[0].digest.is_empty());
        });
    }

    #[test]
    fn every_pass_rebuilds_but_revisions_move_only_on_a_real_change() {
        with_state(
            "app::projection::tests::every_pass_rebuilds_but_revisions_move_only_on_a_real_change",
            |mut state, _| {
                state.config.remote.enabled = true;
                let root = state.launch_dir.clone();
                state.sessions = vec![
                    session("a", AgentKind::Claude, AgentStatus::Working, &root),
                    session("b", AgentKind::Claude, AgentStatus::Idle, &root),
                ];
                state.sessions[0].approvals.push(approval("x1"));
                let mut plan = Plan { id: "p1".into(), goal: "ship".into(), ..Plan::default() };
                plan.tasks = vec![task("t1", Some("a")), task("t2", None)];
                state.plans = vec![plan];
                // A session's task is the first on it in plan order: a later task in the same plan
                // or in a later plan does not take its place.
                let mut later = Plan { id: "p2".into(), ..Plan::default() };
                later.tasks = vec![task("t9", Some("a"))];
                state.plans.push(later);
                state.plans[0].tasks.push(task("t8", Some("a")));
                let link = state.session_infos()[0].task.clone().map(|t| (t.plan, t.task));
                assert_eq!(link, Some(("p1".to_string(), "t1".to_string())));
                state.plans.pop();
                state.plans[0].tasks.pop();
                let (out, bodies) = (Projections::default(), DiffBodies::default());
                let mut cache = DiffSummaries::default();

                state.publish_projections(&out, &mut cache, &bodies);
                assert_eq!(published(&out), built(&state));
                assert_eq!(published(&out).0[0].task.as_ref().map(|t| t.task.as_str()), Some("t1"));

                // Passes that change nothing a client sees move no revision, so clients get no
                // event: neither does a change to something the projection leaves out.
                let quiet = revisions(&out);
                for _ in 0..3 {
                    state.publish_projections(&out, &mut cache, &bodies);
                }
                state.plans[0].tasks[0].notes.push("not on the wire".into());
                state.publish_projections(&out, &mut cache, &bodies);
                assert_eq!(revisions(&out), quiet, "a pass with nothing to tell moved a revision");

                // Each change to something a client sees is published and moves the cursor; the
                // pass after it moves nothing.
                let changes: Vec<(&str, Change)> = vec![
                    ("session title", change(|s| s.sessions[1].title = "renamed".into())),
                    ("session status", change(|s| s.sessions[1].status = AgentStatus::Done)),
                    ("session agent", change(|s| s.sessions[1].agent = "other".into())),
                    ("session chief", change(|s| s.sessions[1].chief = true)),
                    ("session cwd", change(|s| s.sessions[1].cwd = "/elsewhere".into())),
                    ("approval added", change(|s| s.sessions[1].approvals.push(approval("x2")))),
                    ("approval removed", change(|s| s.sessions[1].approvals.clear())),
                    ("session removed", change(|s| drop(s.sessions.pop()))),
                    ("plan goal", change(|s| s.plans[0].goal = "ship it".into())),
                    // Not a field of its own, but part of the approval scope the digest covers.
                    ("task brief", change(|s| s.plans[0].tasks[1].brief = "and the lexer".into())),
                    ("plan approved", change(|s| s.plans[0].approved = true)),
                    ("plan status", change(|s| s.plans[0].status = PlanStatus::Completed)),
                    ("plan integration", change(|s| s.plans[0].integration = Some("int".into()))),
                    ("task title", change(|s| s.plans[0].tasks[1].title = "new".into())),
                    ("task status", change(|s| s.plans[0].tasks[1].status = TaskStatus::Done)),
                    ("task agent", change(|s| s.plans[0].tasks[1].agent = "codex".into())),
                    ("task depends_on", change(|s| s.plans[0].tasks[1].depends_on = vec!["t1".into()])),
                    ("task branch", change(|s| s.plans[0].tasks[1].branch = Some("b".into()))),
                    ("task integrated", change(|s| s.plans[0].tasks[1].integrated = Some("main".into()))),
                    ("task counts", change(|s| s.plans[0].tasks[1].additions = 9)),
                    (
                        "task report",
                        change(|s| {
                            s.plans[0].tasks[1].report =
                                Some(Report { status: "done".into(), summary: "ok".into(), at: 1 })
                        }),
                    ),
                    ("task session (a session's task link)", change(|s| s.plans[0].tasks[0].session = None)),
                    ("task added", change(|s| s.plans[0].tasks.push(task("t3", None)))),
                    ("plan removed", change(|s| s.plans.clear())),
                ];
                for (what, change) in changes {
                    let was = built(&state);
                    change(&mut state);
                    assert_ne!(built(&state), was, "{what}: the change is not visible in the projection");
                    let before = revisions(&out);
                    state.publish_projections(&out, &mut cache, &bodies);
                    assert_eq!(published(&out), built(&state), "{what}: not republished");
                    assert!(revisions(&out).0 > before.0, "{what}: no revision moved");
                    let after = revisions(&out);
                    state.publish_projections(&out, &mut cache, &bodies);
                    assert_eq!(revisions(&out), after, "{what}: a revision moved without a change");
                }

                // A diff's summary moves with its revision; new content at the same revision only
                // replaces the bodies `diff_get` reads.
                let root = std::path::PathBuf::from("/work/tree");
                state.diffs.entry(root.clone()).or_default().prepared = Some(prepared(1));
                let before = revisions(&out);
                state.publish_projections(&out, &mut cache, &bodies);
                assert_eq!(out.diffs.load().data[0].files.len(), 1);
                assert!(revisions(&out).0 > before.0);
                assert!(bodies.file("/work/tree", "src/file1.rs").is_none());
                state.diffs.get_mut(&root).unwrap().prepared = Some(prepared(2));
                let quiet = revisions(&out);
                state.publish_projections(&out, &mut cache, &bodies);
                assert!(bodies.file("/work/tree", "src/file1.rs").is_some(), "new content at the same revision");
                assert_eq!(revisions(&out), quiet);
                state.diffs.get_mut(&root).unwrap().revision = 1;
                state.publish_projections(&out, &mut cache, &bodies);
                assert_eq!(out.diffs.load().data[0].files.len(), 2);
                state.diffs.clear();
                state.publish_projections(&out, &mut cache, &bodies);
                assert!(out.diffs.load().data.is_empty());
            },
        );
    }

    /// The cost of a runtime pass on a large board: `ATOMICS_BENCH_PROJECTION=<file>` writes the numbers.
    #[test]
    fn publishing_cost_on_a_large_board() {
        let Some(report) = std::env::var_os("ATOMICS_BENCH_PROJECTION") else { return };
        with_state("app::projection::tests::publishing_cost_on_a_large_board", move |mut state, _| {
            let root = state.launch_dir.clone();
            state.sessions = (0..400)
                .map(|i| {
                    let mut s = session(&format!("s{i}"), AgentKind::Claude, AgentStatus::Working, &root);
                    s.title = format!("a fairly long session title number {i} to make strings real");
                    if i % 4 == 0 {
                        s.approvals.push(approval(&format!("x{i}")));
                    }
                    s
                })
                .collect();
            state.plans = (0..100)
                .map(|p| Plan {
                    id: format!("plan{p}"),
                    goal: "a plan goal of a typical length for a real board".into(),
                    tasks: (0..20)
                        .map(|t| {
                            let mut task = task(&format!("t{t}"), (t == 0).then(|| format!("s{p}")).as_deref());
                            task.brief = "x".repeat(400);
                            task.report = Some(Report { status: "progress".into(), summary: "y".repeat(200), at: 1 });
                            task
                        })
                        .collect(),
                    ..Plan::default()
                })
                .collect();
            for i in 0..100 {
                let data = state.diffs.entry(format!("/work/tree{i}").into()).or_default();
                data.revision = 1;
                data.prepared = Some(prepared(50));
            }
            let (out, bodies) = (Projections::default(), DiffBodies::default());
            let mut cache = DiffSummaries::default();
            state.publish_projections(&out, &mut cache, &bodies);
            const PASSES: u32 = 300;
            let time = |f: &mut dyn FnMut()| {
                let started = std::time::Instant::now();
                for _ in 0..PASSES {
                    f();
                }
                started.elapsed() / PASSES
            };
            let idle = time(&mut || state.publish_projections(&out, &mut cache, &bodies));
            // One session changes on every pass (its title, as a running agent's would).
            let mut n = 0;
            let busy = time(&mut || {
                n += 1;
                state.sessions[0].title = format!("t{n}");
                state.publish_projections(&out, &mut cache, &bodies);
            });
            std::fs::write(
                report,
                format!(
                    "board: 400 sessions, 100 plans x 20 tasks, 100 approvals, 100 diffs x 50 files\n\
                     per pass, unchanged board: {idle:?}\n\
                     per pass, one session title changed: {busy:?}\n"
                ),
            )
            .unwrap();
        });
    }
}
