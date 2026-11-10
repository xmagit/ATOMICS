use super::*;

impl PendingApproval {
    /// Agent-authored command, reason and input are data. Identity comes from the host's
    /// session/task lookup; at is when the host observed this request for the reply or event.
    pub(super) fn json(&self, task: Option<&Task>, fallback_agent: &str, at: u64) -> serde_json::Value {
        let agent = task
            .map(|t| atomics_core::tickets::model_label(&t.agent, &t.args))
            .unwrap_or_else(|| fallback_agent.to_string());
        let mut value = json!(self.request);
        value["id"] = json!(self.id);
        value["provenance"] = plan::agent_provenance("approval request", task.map(|t| t.id.as_str()), &agent, at);
        value
    }
}

impl State {
    pub(super) fn approval_json(&self, index: usize, pending: &PendingApproval, at: u64) -> serde_json::Value {
        let session = &self.sessions[index];
        let task = self.plans.iter().flat_map(|p| &p.tasks).find(|t| t.session.as_deref() == Some(&session.id));
        pending.json(task, &session.agent, at)
    }

    pub(super) fn approval_poll_interval(&self) -> Option<Duration> {
        if self.sessions.iter().any(|session| session.approval_hold || !session.approvals.is_empty()) {
            Some(Duration::from_millis(400))
        } else if self
            .sessions
            .iter()
            .any(|session| session.status == AgentStatus::NeedsInput && session.terminal.is_some())
        {
            Some(NEEDS_INPUT_CHECK_INTERVAL)
        } else {
            None
        }
    }

    pub(super) fn release_held(&mut self, index: usize) {
        if !self.sessions[index].approval_hold
            && self.sessions[index].approvals.is_empty()
            && !self.sessions[index].pending.as_ref().is_some_and(|pending| pending.codex)
            && !self.sessions[index].held.is_empty()
        {
            self.sessions[index].activity.held_since = None;
            let text = std::mem::take(&mut self.sessions[index].held).join("\n\n");
            self.deliver(index, &text);
        }
    }

    pub(super) fn expire_approvals(&mut self, now: Instant) {
        for approval in self.relay.take_write_approvals() {
            let index = self.sessions.iter().position(|session| session.sandbox.as_deref() == Some(&approval.session));
            if let Some(index) = index.filter(|_| !approval.reply.expired()) {
                let session = &mut self.sessions[index];
                session.approvals.push(PendingApproval {
                    id: uuid::Uuid::new_v4().to_string(),
                    request: approval.request,
                    reply: Some(approval.reply),
                    native: None,
                    stash: StashPins::default(),
                });
                session.status = AgentStatus::NeedsInput;
                self.last_status.remove(&session.id);
            } else {
                approval.reply.answer(HookReply::default());
            }
        }
        self.mirror_pending_permission();
        // A later live reply must never keep an abandoned hook at the head, even between screen polls.
        for index in 0..self.sessions.len() {
            if self.sessions[index].approvals.iter().any(|pending| {
                pending.request.tool != "Azure REST"
                    && pending.native.is_none()
                    && pending.reply.as_ref().is_none_or(|reply| reply.expired())
            }) {
                let screen =
                    self.sessions[index].terminal.as_ref().map(|terminal| terminal.screen_text(60)).unwrap_or_default();
                self.prune_dead_approvals(index, &screen, now);
            }
        }
        if now.duration_since(self.approvals_checked) < APPROVAL_CHECK_INTERVAL {
            return;
        }
        self.approvals_checked = now;
        let check_needs_input = now.duration_since(self.needs_input_checked) >= NEEDS_INPUT_CHECK_INTERVAL;
        if check_needs_input {
            self.needs_input_checked = now;
        }
        for index in 0..self.sessions.len() {
            let session = &mut self.sessions[index];
            if session.sandbox.is_some() {
                let had_approvals = !session.approvals.is_empty();
                let mut expired = Vec::new();
                session.approvals.retain(|pending| {
                    let keep = pending.request.tool == "Azure REST"
                        && pending.reply.as_ref().is_some_and(|reply| !reply.expired());
                    if !keep {
                        expired.push(pending.id.clone());
                    }
                    keep
                });
                if had_approvals && session.approvals.is_empty() {
                    session.status = if session.terminal.as_ref().is_some_and(Terminal::exited) {
                        AgentStatus::Exited
                    } else {
                        AgentStatus::Working
                    };
                }
                session.approval_hold = false;
                if !expired.is_empty() {
                    self.last_status.remove(&session.id);
                }
                for id in expired {
                    self.record_resolution(id, Disposition::Expired);
                }
                continue;
            }
            let pending = session.approval_hold || !session.approvals.is_empty();
            if !pending && !(check_needs_input && session.status == AgentStatus::NeedsInput) {
                continue;
            }
            let screen = session.terminal.as_ref().map(|terminal| terminal.screen_text(60)).unwrap_or_default();
            self.sync_native(index, &screen, now);
            let session = &mut self.sessions[index];
            let pending = session.approval_hold || !session.approvals.is_empty();
            let menu = atomics_core::approvals::menu_visible(&screen);
            if menu {
                // A menu holds input, but never consumes a waiting hook, even for the same command.
                // The IPC waiter expires disconnected hooks before their fallback menu takes over.
                session.approval_hold = true;
                session.menu_absent_since = None;
                continue;
            }
            if session.approvals.iter().any(|pending| pending.reply.as_ref().is_some_and(|reply| !reply.expired())) {
                session.menu_absent_since = None;
                continue;
            }
            if !pending
                && !self
                    .config
                    .agent(&session.agent)
                    .and_then(|agent| agent.busy_text.as_ref())
                    .is_some_and(|busy| screen.contains(busy))
            {
                session.menu_absent_since = None;
                continue;
            }
            let missing_since = session.menu_absent_since.get_or_insert(now);
            if now.duration_since(*missing_since) < APPROVAL_MENU_GRACE {
                continue;
            }
            let expired = std::mem::take(&mut session.approvals);
            session.approval_hold = false;
            session.menu_absent_since = None;
            if session.status == AgentStatus::NeedsInput {
                session.status = if session.terminal.as_ref().is_some_and(Terminal::exited) {
                    AgentStatus::Exited
                } else {
                    AgentStatus::Working
                };
            }
            for pending in expired {
                self.record_resolution(pending.id, Disposition::Expired);
            }
            self.release_held(index);
        }
    }

    /// Drop hooks that cannot be answered and have no corresponding menu when a live reply waits
    /// behind them or another menu is visible. With no menu, keep the quiet period for the fallback
    /// prompt to draw; Azure requests keep the relay's expiry handling above. One menu can stand
    /// for only one expired request, or for the answer still lingering on screen.
    fn prune_dead_approvals(&mut self, index: usize, screen: &str, now: Instant) {
        let session = &mut self.sessions[index];
        let menu = session.sandbox.is_none() && atomics_core::approvals::menu_visible(screen);
        let command_menu = if menu { atomics_core::approvals::native_menu(screen) } else { None };
        let mut represented = command_menu.as_ref().is_some_and(|visible| {
            session.answered_menu.as_ref().is_some_and(|(answered, at)| {
                now.saturating_duration_since(*at) < ANSWERED_MENU_LINGER && visible.same(answered)
            }) || session.approvals.iter().any(|pending| pending.native.as_ref().is_some_and(|menu| visible.same(menu)))
        });
        let live_reply =
            session.approvals.iter().any(|pending| pending.reply.as_ref().is_some_and(|reply| !reply.expired()));
        let mut expired = Vec::new();
        session.approvals.retain(|pending| {
            let live = pending.request.tool == "Azure REST"
                || pending.native.is_some()
                || pending.reply.as_ref().is_some_and(|reply| !reply.expired());
            if live {
                return true;
            }
            let on_screen = !represented
                && command_menu.is_some()
                && [Decision::Allow, Decision::Deny].into_iter().any(|decision| {
                    atomics_core::approvals::menu_key(screen, pending.request.command.as_deref(), decision).is_some()
                });
            represented |= on_screen;
            if on_screen || (!menu && !live_reply) {
                return true;
            }
            expired.push(pending.id.clone());
            false
        });
        if expired.is_empty() {
            return;
        }
        // Wake the chief even if the session still needs input: the first live request is now shown.
        self.last_status.remove(&session.id);
        for id in expired {
            self.record_resolution(id, Disposition::Expired);
        }
    }

    /// Bring a session's approvals in line with the command menu its agent drew with no hook asking:
    /// one pending approval for the menu on screen, under one ID while it stays, so `atomics_approve`,
    /// raw keys, the tile, the board and every client go by the same list. A hook request comes
    /// first; the screen only stands in for a missing one. Another menu replacing the command menu
    /// withdraws its approval at once; a screen with no menu leaves it to the quiet period.
    pub(super) fn sync_native(&mut self, index: usize, screen: &str, now: Instant) {
        let session = &mut self.sessions[index];
        if session.sandbox.is_some() || !session.is_agent() || session.terminal.is_none() {
            return;
        }
        let visible = atomics_core::approvals::native_menu(screen);
        if let Some((answered, at)) = &session.answered_menu
            && (now.saturating_duration_since(*at) >= ANSWERED_MENU_LINGER
                || !visible.as_ref().is_some_and(|visible| visible.same(answered)))
        {
            session.answered_menu = None;
        }
        let shown = match session.approvals.first() {
            None => None,
            Some(pending) => match &pending.native {
                Some(menu) => Some(menu),
                None => return,
            },
        };
        match (shown, &visible) {
            (Some(shown), Some(visible)) if shown.same(visible) => return,
            (Some(_), None) if !atomics_core::approvals::menu_visible(screen) => return,
            (None, None) => return,
            _ => {}
        }
        // Withdrawn: a decision still naming it finds it no longer pending. Hooks behind it
        // keep waiting for their own replies when the menu changes.
        session.approvals.retain(|pending| pending.native.is_none());
        if !session.approvals.is_empty() {
            self.last_status.remove(&session.id);
            return;
        }
        let asked = visible
            .filter(|visible| !session.answered_menu.as_ref().is_some_and(|(answered, _)| answered.same(visible)));
        if let Some(menu) = asked {
            let request = ApprovalRequest {
                tool: "Bash".into(),
                command: menu.command.clone(),
                cwd: session.cwd.clone(),
                reason: menu.reason.clone(),
                input: json!({}),
                warning: None,
            };
            // A stash drop on the agent's own menu says whose entries it takes, as a hook's request does.
            let (warning, stash) = self.stash_warning(index, &request).unzip();
            let session = &mut self.sessions[index];
            session.approvals.push(PendingApproval {
                id: uuid::Uuid::new_v4().to_string(),
                request: ApprovalRequest { warning, ..request },
                reply: None,
                native: Some(menu),
                stash: stash.unwrap_or_default(),
            });
            session.status = AgentStatus::NeedsInput;
            session.approval_hold = true;
            session.menu_absent_since = None;
            // A new event carries the request to waiters even when the session already needed input.
            self.last_status.remove(&session.id);
        }
    }

    /// Whether `atomics_approve` can answer the session's permission prompt now, read afresh: a hook
    /// waits for its reply, or the agent's menu for the first request is on screen. Raw keys are
    /// refused only then, so whoever decides always has one way to answer.
    pub(super) fn approvable(&mut self, index: usize) -> bool {
        let screen =
            self.sessions[index].terminal.as_ref().map(|terminal| terminal.screen_text(60)).unwrap_or_default();
        let now = Instant::now();
        self.prune_dead_approvals(index, &screen, now);
        self.sync_native(index, &screen, now);
        let Some(pending) = self.sessions[index].approvals.first() else { return false };
        if pending.reply.as_ref().is_some_and(|reply| !reply.expired()) {
            return true;
        }
        [Decision::Allow, Decision::Deny].into_iter().any(|decision| match &pending.native {
            Some(menu) => atomics_core::approvals::native_key(&screen, menu, decision).is_some(),
            None => atomics_core::approvals::menu_key(&screen, pending.request.command.as_deref(), decision).is_some(),
        })
    }

    /// Answer the approval with this ID, in whichever session it waits. Its ID belongs to that one
    /// request, so a decision made for it never answers the next one. Protocol clients, the
    /// desktop and ctl all decide on this thread, one at a time: the first decision wins, and a
    /// later one finds it answered and hears how (`already_resolved`).
    pub(super) fn decide(
        &mut self,
        approval: &str,
        decision: Decision,
        message: Option<&str>,
    ) -> Result<Disposition, WireError> {
        self.expire_approvals(Instant::now());
        let waiting = self.sessions.iter().enumerate().find_map(|(index, session)| {
            session.approvals.iter().position(|pending| pending.id == approval).map(|at| (index, at))
        });
        match waiting {
            None => Err(match self.resolution(approval) {
                Some(disposition) => WireError::new(
                    ErrorCode::AlreadyResolved,
                    format!("that permission request was already answered: {}", disposition.label()),
                ),
                None => WireError::new(ErrorCode::NotFound, "that permission request is no longer pending"),
            }),
            Some((_, at)) if at > 0 => Err(WireError::new(
                ErrorCode::Rejected,
                "an earlier permission request in that session waits for an answer first",
            )),
            // How it ended, as `approve` recorded it: "Allow always" can end as a one-time allow.
            Some((index, _)) => self
                .approve(index, approval, decision, message)
                .map(|_| self.resolution(approval).unwrap_or(disposition(decision))),
        }
    }

    /// What an approval of `request` from the session at `index` warns about when its command drops
    /// shared stash entries: those entries as git lists them now, in the repository the command runs
    /// git in, and whose they are; and the same entries by commit, which approving checks again.
    pub(super) fn stash_warning(&self, index: usize, request: &ApprovalRequest) -> Option<(String, StashPins)> {
        use atomics_core::stash;
        let drops = request.stash_drops();
        if drops.is_empty() {
            return None;
        }
        let base = if request.cwd.as_os_str().is_empty() { self.sessions[index].root() } else { request.cwd.clone() };
        // Where each drop runs git: the folder its `cd`s and `git -C` lead to, and the repository a
        // `--git-dir` or `GIT_DIR` names from there. The shell expands a leading `~` in both.
        let dirs: Vec<(PathBuf, Option<PathBuf>)> = drops
            .iter()
            .map(|drop| {
                let dir = drop.dirs.iter().fold(base.clone(), |dir, d| dir.join(paths::expand(d)));
                (dir, drop.git_dir.as_deref().map(paths::expand))
            })
            .collect();
        type Place = (PathBuf, Option<PathBuf>);
        let mut listed: Vec<(&Place, Result<Vec<stash::Entry>, String>)> = Vec::new();
        for place in &dirs {
            if !listed.iter().any(|(listed, _)| *listed == place) {
                let (dir, git_dir) = place;
                listed.push((place, stash::list_in(dir, git_dir.as_deref()).map_err(|error| format!("{error:#}"))));
            }
        }
        let entries = |place: &Place| &listed.iter().find(|(listed, _)| *listed == place).expect("listed").1;
        let drops: Vec<_> = drops
            .iter()
            .zip(&dirs)
            .map(|(drop, dir)| (drop, entries(dir).as_deref().map_err(String::as_str)))
            .collect();
        let pins = drops.iter().map(|(drop, listed)| listed.ok().map(|entries| stash::taken(drop, entries))).collect();
        Some((stash::warning(drops, |entry| self.stash_owner(index, entry)), StashPins(pins)))
    }

    /// Lists the stash again before the waiting request `approval` of the session at `index` is let
    /// through, when its command drops stash entries: a push or drop in any checkout since it asked
    /// moves `stash@{n}` to another entry, perhaps another agent's. When a drop would now take other
    /// entries than its warning named, nothing is answered. The request waits on under a new ID with
    /// the warning as it reads now (so a decision made for the old one never answers it), every
    /// waiter hears of it, and the decision is refused saying what changed.
    fn recheck_stash(&mut self, index: usize, approval: &str) -> Result<(), WireError> {
        let session = &self.sessions[index];
        let Some(pending) = session.approvals.first().filter(|pending| pending.id == approval) else { return Ok(()) };
        if pending.stash.is_empty() {
            return Ok(());
        }
        let Some((warning, pins)) = self.stash_warning(index, &pending.request) else { return Ok(()) };
        if pins == pending.stash {
            return Ok(());
        }
        let session = &mut self.sessions[index];
        let pending = &mut session.approvals[0];
        pending.id = uuid::Uuid::new_v4().to_string();
        pending.request.warning = Some(warning.clone());
        pending.stash = pins;
        self.last_status.remove(&session.id);
        let now: Vec<&str> = warning.lines().filter(|line| *line != atomics_core::stash::SHARED).collect();
        Err(WireError::new(
            ErrorCode::StaleRevision,
            format!(
                "the shared stash changed while this request waited, so it was not answered. Now: {} \
                 Review the request again: it waits under a new ID.",
                now.join(" ")
            ),
        ))
    }

    /// Whose stash `entry` is: the session whose worktree has its branch checked out, or else whose
    /// task its message names (`git stash push -m <task id>`); a task whose session has ended, by its
    /// branch or id; `requester` itself when it is that session.
    fn stash_owner(&self, requester: usize, entry: &atomics_core::stash::Entry) -> atomics_core::stash::Owner {
        use atomics_core::stash::Owner;
        let branch = entry.branch();
        let tag = entry.message().and_then(|message| message.split_whitespace().next());
        let tasks = || self.plans.iter().flat_map(|plan| &plan.tasks);
        let task_of = |session: &Session| tasks().find(|task| task.session.as_deref() == Some(&session.id));
        let by_branch: Vec<usize> =
            (0..self.sessions.len()).filter(|&i| branch.is_some() && self.sessions[i].branch() == branch).collect();
        let owners = if by_branch.is_empty() {
            (0..self.sessions.len())
                .filter(|&i| tag.is_some_and(|tag| task_of(&self.sessions[i]).is_some_and(|task| task.id == tag)))
                .collect()
        } else {
            by_branch
        };
        if owners.contains(&requester) {
            return Owner::Requester;
        }
        if let Some(&owner) = owners.first() {
            let session = &self.sessions[owner];
            return Owner::Known(match task_of(session) {
                _ if session.chief => "the main checkout, the chief's or yours".into(),
                Some(task) => format!("task {} (session {})", task.id, session.id),
                None => format!("session {}", session.id),
            });
        }
        let ended: Vec<_> = tasks()
            .filter(|task| {
                (branch.is_some() && task.branch.as_deref() == branch) || tag.is_some_and(|tag| task.id == tag)
            })
            .collect();
        match ended[..] {
            [task] => Owner::Known(format!("task {}", task.id)),
            _ => Owner::Unknown,
        }
    }

    /// How the approval `id` ended, if it was answered lately.
    pub(super) fn resolution(&self, id: &str) -> Option<Disposition> {
        self.resolved.iter().rev().find(|(i, _)| i == id).map(|(_, d)| *d)
    }

    fn record_resolution(&mut self, id: String, disposition: Disposition) {
        self.resolved.push_back((id, disposition));
        while self.resolved.len() > atomics_core::projection::MAX_RESOLUTIONS {
            self.resolved.pop_front();
        }
    }

    /// Answer the session's first approval, through its hook or with the keys of the menu on screen,
    /// and say what was answered.
    pub(super) fn approve(
        &mut self,
        index: usize,
        approval: &str,
        decision: Decision,
        message: Option<&str>,
    ) -> Result<Answered, WireError> {
        let rejected = |message: &str| WireError::new(ErrorCode::Rejected, message);
        self.expire_approvals(Instant::now());
        self.approvable(index);
        // A drop is let through only while it takes the entries its warning named; a denial drops nothing.
        if decision != Decision::Deny {
            self.recheck_stash(index, approval)?;
        }
        let session = &mut self.sessions[index];
        let Some(pending) = session.approvals.first_mut() else {
            return Err(WireError::new(ErrorCode::NotFound, "no pending permission request"));
        };
        if pending.id != approval {
            return Err(WireError::new(
                ErrorCode::StaleRevision,
                "stale permission decision: review the current request",
            ));
        }
        // "Allow always" answers once a request ATOMICS must not remember, such as a stash drop.
        let decision = match decision {
            Decision::AllowAlways if !pending.request.may_allow_always() => Decision::Allow,
            decision => decision,
        };
        let reply = HookReply { stdout: Some(decision.output(message)) };
        let azure = pending.request.tool == "Azure REST";
        let answered = pending.reply.take().is_some_and(|sender| sender.answer(reply));
        if !answered {
            if azure {
                let expired = session.approvals.remove(0);
                self.record_resolution(expired.id, Disposition::Expired);
                return Err(rejected("the Azure request expired; retry the request"));
            }
            let Some(terminal) = &session.terminal else {
                return Err(rejected("the permission hook has returned; answer in the agent terminal"));
            };
            let screen = terminal.screen_text(60);
            let key = match &pending.native {
                Some(menu) => atomics_core::approvals::native_key(&screen, menu, decision).ok_or_else(|| {
                    let shown = atomics_core::approvals::native_menu(&screen).is_some_and(|shown| shown.same(menu));
                    if shown {
                        rejected("the menu on screen offers no such choice now; answer it in the agent terminal")
                    } else {
                        WireError::new(
                            ErrorCode::StaleRevision,
                            "the permission menu changed; review the current request",
                        )
                    }
                })?,
                None => atomics_core::approvals::menu_key(&screen, pending.request.command.as_deref(), decision)
                    .ok_or_else(|| rejected("the permission menu is not visible; retry or answer in the terminal"))?,
            };
            terminal.write(key.as_bytes().to_vec());
            if key == "\x01" {
                return Err(rejected("expanded the permission command; retry approval once it is visible"));
            }
            if let Some(menu) = atomics_core::approvals::native_menu(&screen) {
                session.answered_menu = Some((menu, Instant::now()));
            }
        }
        let pending = session.approvals.remove(0);
        let id = pending.id.clone();
        let answered = Answered { request: pending.request.clone(), menu: pending.native.map(|menu| menu.kind) };
        // A menu read off the screen answers only itself; a hook's exact request may be allowed again,
        // unless it may not be allowed always (an Azure request, a stash drop), which answered it once above.
        if decision == Decision::AllowAlways && answered.menu.is_none() {
            session.allowed.push(pending.request);
        }
        session.status = if session.approvals.is_empty() { AgentStatus::Working } else { AgentStatus::NeedsInput };
        // Waiters must hear about the next request even when the status stays "needs you".
        if !session.approvals.is_empty() {
            self.last_status.remove(&session.id);
        }
        self.record_resolution(id, disposition(decision));
        Ok(answered)
    }
}

/// The stash entries a waiting request's warning named, by commit: one list per drop its command
/// makes, or `None` where that stash could not be listed. Empty for a request that drops none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct StashPins(Vec<Option<Vec<String>>>);

impl StashPins {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// How long a menu just answered with keys may stay on screen before it counts as asked again.
const ANSWERED_MENU_LINGER: Duration = Duration::from_secs(3);

/// A permission request just answered, and the agent's own menu when its keys answered it.
#[derive(Debug)]
pub(super) struct Answered {
    pub request: ApprovalRequest,
    pub menu: Option<atomics_core::approvals::MenuKind>,
}

impl Answered {
    /// What `atomics_approve` echoes, so whoever decided sees the command the answer let through.
    pub(super) fn json(&self) -> serde_json::Value {
        json!({
            "answered": true,
            "tool": self.request.tool,
            "command": self.request.command,
            "reason": self.request.reason,
            "menu": self.menu.map(|menu| menu.label()),
        })
    }
}

fn disposition(decision: Decision) -> Disposition {
    match decision {
        Decision::Allow => Disposition::Allowed,
        Decision::AllowAlways => Disposition::AllowedAlways,
        Decision::Deny => Disposition::Denied,
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{answer_pending, request, session, with_app};
    use super::*;

    /// A hookless agent drawing the recorded screens in `screens`, in order, whose terminal then
    /// lists the keys it got (see tests/fixtures/approval-menu).
    fn menu_agent(app: &Desktop<'_>, screens: &[&Path]) -> Terminal {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/approval-menu");
        let args = screens.iter().map(|screen| screen.to_string_lossy().into_owned()).collect();
        let terminal = Terminal::spawn(
            app.runtime.wake.clone(),
            app.runtime.theme.terminal_colors(),
            script.to_string_lossy().into_owned(),
            args,
            &app.runtime.launch_dir,
            HashMap::new(),
            100,
        )
        .unwrap();
        shows(&terminal, "1. Yes");
        terminal
    }

    fn shows(terminal: &Terminal, text: &str) {
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
        while !terminal.screen_text(60).contains(text) {
            assert!(Instant::now() < deadline, "{text:?} never showed:\n{}", terminal.screen_text(60));
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn recorded(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../atomics-core/tests/fixtures").join(name)
    }

    /// Lets the next poll read the screen, past the quiet period of a menu that went away.
    fn poll_after_quiet(app: &mut Desktop<'_>) {
        for absent in [None, Some(Instant::now() - APPROVAL_MENU_GRACE)] {
            if absent.is_some() {
                app.runtime.sessions[0].menu_absent_since = absent;
            }
            app.runtime.approvals_checked = Instant::now() - APPROVAL_CHECK_INTERVAL;
            app.runtime.expire_approvals(Instant::now());
        }
    }

    fn hook_request(app: &mut Desktop<'_>, id: &str) -> Arc<ipc::ApprovalReply> {
        let reply = Arc::new(ipc::ApprovalReply::default());
        app.runtime.sessions[0].approvals.push(PendingApproval {
            id: id.into(),
            request: ApprovalRequest {
                tool: "Bash".into(),
                command: Some(format!("touch {id}")),
                cwd: app.runtime.launch_dir.clone(),
                reason: None,
                input: json!({}),
                warning: None,
            },
            reply: Some(reply.clone()),
            native: None,
            stash: Default::default(),
        });
        reply
    }

    #[test]
    fn review_an_unrelated_choice_menu_must_not_keep_a_dead_hook_ahead_of_a_live_hook() {
        with_app(
            "app::approvals::tests::review_an_unrelated_choice_menu_must_not_keep_a_dead_hook_ahead_of_a_live_hook",
            |app, _| {
                let dir = crate::test_support::tempdir().unwrap();
                let screen = dir.path().join("edit-menu.txt");
                std::fs::write(
                    &screen,
                    "Allow these file edits?\n› 1. Yes, proceed\n  2. No\nPress enter to confirm or esc to cancel\n",
                )
                .unwrap();
                let terminal = menu_agent(app, &[&screen]);
                let mut worker =
                    session("worker", AgentKind::Generic, AgentStatus::NeedsInput, &app.runtime.launch_dir);
                worker.terminal = Some(terminal);
                worker.approval_hold = true;
                app.runtime.sessions = vec![worker];
                hook_request(app, "dead").wait(Duration::ZERO);
                let live = hook_request(app, "live");
                assert_eq!(app.runtime.deliver(0, "held steering"), Delivery::Held);
                app.runtime.approvals_checked = Instant::now() - APPROVAL_CHECK_INTERVAL;
                let result = app.runtime.decide("live", Decision::Allow, None);
                assert!(result.is_ok(), "a dead request blocked a live reply: {result:?}");
                assert!(live.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
                assert_eq!(app.runtime.resolution("dead"), Some(Disposition::Expired));
                assert!(app.runtime.sessions[0].approval_hold);
                assert_eq!(app.runtime.sessions[0].held, ["held steering"]);
            },
        );
    }

    #[test]
    fn a_command_menu_keeps_live_hooks_until_their_own_decisions() {
        with_app("app::approvals::tests::a_command_menu_keeps_live_hooks_until_their_own_decisions", |app, _| {
            for (fixture, command, keys) in [
                ("codex-0.165-approval-screen.txt", "atomics queue --class test -- ./check.sh", "Keys: 31 0d"),
                ("claude-2.1-approval-screen.txt", "touch fixture", "Keys: 0d"),
            ] {
                let dir = crate::test_support::tempdir().unwrap();
                let screen = dir.path().join("command-menu.txt");
                std::fs::write(
                    &screen,
                    std::fs::read_to_string(recorded(fixture)).unwrap().replace(command, "touch shown"),
                )
                .unwrap();
                let mut worker =
                    session("worker", AgentKind::Generic, AgentStatus::NeedsInput, &app.runtime.launch_dir);
                worker.terminal = Some(menu_agent(app, &[&screen]));
                app.runtime.sessions = vec![worker];
                let shown = hook_request(app, "shown");
                let live = hook_request(app, "live");
                let commandless = hook_request(app, "commandless");
                app.runtime.sessions[0].approvals.last_mut().unwrap().request.command = None;
                app.runtime.approvals_checked = Instant::now() - APPROVAL_CHECK_INTERVAL;
                app.runtime.expire_approvals(Instant::now());
                assert!(app.runtime.sessions[0].approvals[0].reply.is_some(), "{fixture}");
                assert!(!shown.expired(), "even the shown command's hook must stay live: {fixture}");
                assert!(!live.expired(), "a different command's hook must stay live: {fixture}");
                assert!(!commandless.expired(), "a hook without a command cannot match: {fixture}");
                assert!(app.runtime.sessions[0].approval_hold);
                assert_eq!(app.runtime.deliver(0, "held steering"), Delivery::Held);
                // A disconnected hook expires before the agent falls back to the menu.
                assert!(shown.wait(Duration::ZERO).stdout.is_none());
                assert_eq!(app.runtime.decide("shown", Decision::Allow, None).unwrap(), Disposition::Allowed);
                shows(app.runtime.sessions[0].terminal.as_ref().unwrap(), keys);
                assert_eq!(app.runtime.decide("live", Decision::Allow, None).unwrap(), Disposition::Allowed);
                assert!(live.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
                assert_eq!(app.runtime.decide("commandless", Decision::Deny, None).unwrap(), Disposition::Denied);
                assert!(commandless.wait(Duration::ZERO).stdout.unwrap().contains("deny"));
                assert!(app.runtime.sessions[0].approval_hold);
                assert_eq!(app.runtime.sessions[0].held, ["held steering"]);
            }
        });
    }

    #[test]
    fn review_a_fallback_menu_must_not_abandon_a_later_live_duplicate_command() {
        with_app(
            "app::approvals::tests::review_a_fallback_menu_must_not_abandon_a_later_live_duplicate_command",
            |app, _| {
                let dir = crate::test_support::tempdir().unwrap();
                let screen = dir.path().join("duplicate-command-menu.txt");
                let menu = std::fs::read_to_string(recorded("codex-0.165-approval-screen.txt"))
                    .unwrap()
                    .replace("atomics queue --class test -- ./check.sh", "touch shown");
                std::fs::write(&screen, menu).unwrap();
                let mut worker =
                    session("worker", AgentKind::Generic, AgentStatus::NeedsInput, &app.runtime.launch_dir);
                worker.terminal = Some(menu_agent(app, &[&screen]));
                worker.approval_hold = true;
                app.runtime.sessions = vec![worker];
                hook_request(app, "shown").wait(Duration::ZERO);
                let live = hook_request(app, "live");
                app.runtime.sessions[0].approvals[1].request.command = Some("touch shown".into());
                app.runtime.approvals_checked = Instant::now() - APPROVAL_CHECK_INTERVAL;
                app.runtime.expire_approvals(Instant::now());
                assert!(
                    app.runtime.sessions[0].approvals[1].reply.is_some(),
                    "the first hook's fallback menu consumed the later live duplicate's reply"
                );
                assert!(
                    live.answer(HookReply { stdout: Some(Decision::Allow.output(None)) }),
                    "the later duplicate still needs its own hook decision"
                );
            },
        );
    }

    #[test]
    fn one_menu_represents_only_one_expired_duplicate_command() {
        with_app("app::approvals::tests::one_menu_represents_only_one_expired_duplicate_command", |app, _| {
            let dir = crate::test_support::tempdir().unwrap();
            let screen = dir.path().join("duplicate-command-menu.txt");
            let menu = std::fs::read_to_string(recorded("codex-0.165-approval-screen.txt"))
                .unwrap()
                .replace("atomics queue --class test -- ./check.sh", "touch shown");
            std::fs::write(&screen, menu).unwrap();
            let mut worker = session("worker", AgentKind::Generic, AgentStatus::NeedsInput, &app.runtime.launch_dir);
            worker.terminal = Some(menu_agent(app, &[&screen]));
            worker.approval_hold = true;
            app.runtime.sessions = vec![worker];
            for id in ["shown", "expired-one", "expired-two"] {
                hook_request(app, id).wait(Duration::ZERO);
                app.runtime.sessions[0].approvals.last_mut().unwrap().request.command = Some("touch shown".into());
            }
            let live = hook_request(app, "live");
            app.runtime.sessions[0].approvals.last_mut().unwrap().request.command = Some("touch shown".into());
            for _ in 0..2 {
                app.runtime.approvals_checked = Instant::now() - APPROVAL_CHECK_INTERVAL;
                app.runtime.expire_approvals(Instant::now());
                assert!(app.runtime.approvable(0));
                assert_eq!(
                    app.runtime.sessions[0].approvals.iter().map(|pending| pending.id.as_str()).collect::<Vec<_>>(),
                    ["shown", "live"]
                );
                assert!(!live.expired());
            }
            assert_eq!(app.runtime.resolution("shown"), None);
            for id in ["expired-one", "expired-two"] {
                assert_eq!(app.runtime.resolution(id), Some(Disposition::Expired));
            }
            assert_eq!(app.runtime.decide("shown", Decision::Deny, None).unwrap(), Disposition::Denied);
            shows(app.runtime.sessions[0].terminal.as_ref().unwrap(), "Keys: 1b");
            assert_eq!(app.runtime.decide("live", Decision::Allow, None).unwrap(), Disposition::Allowed);
            assert!(live.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
        });
    }

    #[test]
    fn an_answered_menu_lingering_beside_a_live_identical_command_keeps_its_reply() {
        with_app(
            "app::approvals::tests::an_answered_menu_lingering_beside_a_live_identical_command_keeps_its_reply",
            |app, _| {
                for fallback in [false, true] {
                    let dir = crate::test_support::tempdir().unwrap();
                    let screen = dir.path().join("lingering-command-menu.txt");
                    let menu = std::fs::read_to_string(recorded("codex-0.165-approval-screen.txt"))
                        .unwrap()
                        .replace("atomics queue --class test -- ./check.sh", "touch shown");
                    std::fs::write(&screen, &menu).unwrap();
                    let mut worker =
                        session("worker", AgentKind::Generic, AgentStatus::NeedsInput, &app.runtime.launch_dir);
                    worker.terminal = Some(menu_agent(app, &[&screen]));
                    app.runtime.sessions = vec![worker];
                    if fallback {
                        hook_request(app, "shown").wait(Duration::ZERO);
                    }
                    assert!(app.runtime.approvable(0));
                    let shown = app.runtime.sessions[0].approvals[0].id.clone();
                    assert_eq!(app.runtime.deliver(0, "held steering"), Delivery::Held);
                    assert_eq!(app.runtime.decide(&shown, Decision::Allow, None).unwrap(), Disposition::Allowed);
                    shows(app.runtime.sessions[0].terminal.as_ref().unwrap(), "Keys: 31 0d");
                    assert!(app.runtime.sessions[0].answered_menu.is_some());

                    // Redraw the old screen to make the agent's delayed repaint deterministic.
                    app.runtime.sessions[0].terminal = Some(menu_agent(app, &[&screen]));
                    app.runtime.sessions[0].answered_menu.as_mut().unwrap().1 = Instant::now();
                    let live = hook_request(app, "live");
                    app.runtime.sessions[0].approvals.last_mut().unwrap().request.command = Some("touch shown".into());
                    hook_request(app, "expired").wait(Duration::ZERO);
                    app.runtime.sessions[0].approvals.last_mut().unwrap().request.command = Some("touch shown".into());
                    for _ in 0..2 {
                        app.runtime.approvals_checked = Instant::now() - APPROVAL_CHECK_INTERVAL;
                        app.runtime.expire_approvals(Instant::now());
                        assert_eq!(app.runtime.sessions[0].approvals.len(), 1);
                        assert_eq!(app.runtime.sessions[0].approvals[0].id, "live");
                        assert!(app.runtime.sessions[0].approvals[0].reply.is_some());
                        assert!(!live.expired());
                    }
                    assert_eq!(app.runtime.resolution("expired"), Some(Disposition::Expired));
                    assert_eq!(app.runtime.resolution(&shown), Some(Disposition::Allowed));
                    assert_eq!(app.runtime.decide("live", Decision::Deny, None).unwrap(), Disposition::Denied);
                    assert!(live.wait(Duration::ZERO).stdout.unwrap().contains("deny"));
                    assert!(app.runtime.sessions[0].terminal.as_ref().unwrap().screen_text(60).contains("1. Yes"));
                    assert!(app.runtime.sessions[0].approval_hold);
                    assert_eq!(app.runtime.sessions[0].held, ["held steering"]);
                }
            },
        );
    }

    #[test]
    fn replacing_a_native_menu_keeps_the_live_hook_behind_it() {
        with_app("app::approvals::tests::replacing_a_native_menu_keeps_the_live_hook_behind_it", |app, _| {
            let mut worker = session("worker", AgentKind::Generic, AgentStatus::NeedsInput, &app.runtime.launch_dir);
            worker.terminal = Some(menu_agent(app, &[&recorded("codex-0.165-approval-screen.txt")]));
            app.runtime.sessions = vec![worker];
            assert!(app.runtime.approvable(0));
            assert!(app.runtime.sessions[0].approvals[0].native.is_some());
            let live = hook_request(app, "live");
            app.runtime.sessions[0].terminal = Some(menu_agent(app, &[&recorded("claude-2.1-approval-screen.txt")]));
            app.runtime.approvals_checked = Instant::now() - APPROVAL_CHECK_INTERVAL;
            app.runtime.expire_approvals(Instant::now());
            assert_eq!(app.runtime.sessions[0].approvals.len(), 1);
            assert_eq!(app.runtime.sessions[0].approvals[0].id, "live");
            assert!(!live.expired());
            assert_eq!(app.runtime.decide("live", Decision::Allow, None).unwrap(), Disposition::Allowed);
            assert!(live.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
            assert!(app.runtime.sessions[0].approval_hold);
        });
    }

    #[test]
    fn a_hidden_command_menu_keeps_live_hook_replies() {
        with_app("app::approvals::tests::a_hidden_command_menu_keeps_live_hook_replies", |app, _| {
            let dir = crate::test_support::tempdir().unwrap();
            let screen = dir.path().join("hidden-command-menu.txt");
            let menu = std::fs::read_to_string(recorded("codex-0.165-approval-screen.txt"))
                .unwrap()
                .replace("$ atomics queue --class test -- ./check.sh", "[… 9 lines] ⌃a view all");
            std::fs::write(&screen, menu).unwrap();
            let mut worker = session("worker", AgentKind::Generic, AgentStatus::NeedsInput, &app.runtime.launch_dir);
            worker.terminal = Some(menu_agent(app, &[&screen]));
            app.runtime.sessions = vec![worker];
            let live = hook_request(app, "live");
            app.runtime.approvals_checked = Instant::now() - APPROVAL_CHECK_INTERVAL;
            app.runtime.expire_approvals(Instant::now());
            assert!(!live.expired(), "a hidden command cannot identify an abandoned hook");
            assert!(app.runtime.sessions[0].approval_hold);
            assert_eq!(app.runtime.decide("live", Decision::Allow, None).unwrap(), Disposition::Allowed);
            assert!(live.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
        });
    }

    #[test]
    fn review_expired_hook_keeps_input_held_until_the_fallback_menu_can_draw() {
        with_app(
            "app::approvals::tests::review_expired_hook_keeps_input_held_until_the_fallback_menu_can_draw",
            |app, _| {
                let terminal = Terminal::spawn(
                    app.runtime.wake.clone(),
                    app.runtime.theme.terminal_colors(),
                    crate::test_support::program("cat"),
                    vec![],
                    &app.runtime.launch_dir,
                    HashMap::new(),
                    100,
                )
                .unwrap();
                let mut worker =
                    session("worker", AgentKind::Generic, AgentStatus::NeedsInput, &app.runtime.launch_dir);
                worker.terminal = Some(terminal);
                worker.approval_hold = true;
                app.runtime.sessions = vec![worker];
                let reply = hook_request(app, "fallback");
                assert_eq!(app.runtime.deliver(0, "held steering"), Delivery::Held);
                reply.wait(Duration::ZERO);
                app.runtime.approvals_checked = Instant::now();
                app.runtime.expire_approvals(Instant::now());
                assert!(app.runtime.sessions[0].approval_hold, "the hook timed out before its native menu could draw");
                assert_eq!(app.runtime.sessions[0].held, ["held steering"]);
                assert!(!app.runtime.approvable(0), "checking approvability must preserve the grace period too");
                assert_eq!(app.runtime.sessions[0].approvals[0].id, "fallback");
                assert_eq!(app.runtime.resolution("fallback"), None);
                app.runtime.approvals_checked = Instant::now() - APPROVAL_CHECK_INTERVAL;
                app.runtime.expire_approvals(Instant::now());
                assert!(app.runtime.sessions[0].menu_absent_since.is_some());
                assert_eq!(app.runtime.sessions[0].held, ["held steering"]);

                // The fallback arrives during the quiet period and still answers the hook's own ID.
                let dir = crate::test_support::tempdir().unwrap();
                let screen = dir.path().join("fallback.txt");
                let menu = std::fs::read_to_string(recorded("codex-0.165-approval-screen.txt"))
                    .unwrap()
                    .replace("atomics queue --class test -- ./check.sh", "touch fallback");
                std::fs::write(&screen, menu).unwrap();
                app.runtime.sessions[0].terminal = Some(menu_agent(app, &[&screen]));
                assert!(app.runtime.approvable(0));
                app.runtime.approvals_checked = Instant::now() - APPROVAL_CHECK_INTERVAL;
                app.runtime.expire_approvals(Instant::now());
                assert_eq!(app.runtime.sessions[0].approvals[0].id, "fallback");
                assert!(app.runtime.sessions[0].menu_absent_since.is_none());
                assert_eq!(app.runtime.sessions[0].held, ["held steering"]);
                assert_eq!(app.runtime.decide("fallback", Decision::Allow, None).unwrap(), Disposition::Allowed);
                shows(app.runtime.sessions[0].terminal.as_ref().unwrap(), "Keys: 31 0d");
                poll_after_quiet(app);
                assert!(app.runtime.sessions[0].held.is_empty());
                assert!(!app.runtime.sessions[0].approval_hold);
            },
        );
    }

    #[test]
    fn dead_hook_heads_expire_and_the_first_live_request_can_be_approved() {
        with_app(
            "app::approvals::tests::dead_hook_heads_expire_and_the_first_live_request_can_be_approved",
            |app, _| {
                for reader in ["poll", "approvable", "approve", "decide", "control"] {
                    app.runtime.sessions =
                        vec![session("worker", AgentKind::Generic, AgentStatus::NeedsInput, &app.runtime.launch_dir)];
                    app.runtime.sessions[0].approval_hold = true;
                    hook_request(app, "dead").wait(Duration::ZERO);
                    let live = hook_request(app, "live");
                    // The fast paths must prune even when the periodic screen check is not due yet.
                    app.runtime.approvals_checked = Instant::now();
                    app.runtime.record_events();
                    let cursor = app.runtime.event_seq;
                    match reader {
                        "poll" => app.runtime.expire_approvals(Instant::now()),
                        "approvable" => assert!(app.runtime.approvable(0)),
                        "approve" => {
                            app.runtime.approve(0, "live", Decision::Allow, None).unwrap();
                        }
                        "decide" => {
                            assert_eq!(app.runtime.decide("live", Decision::Allow, None).unwrap(), Disposition::Allowed)
                        }
                        "control" => {
                            let answer =
                                answer_pending(app, json!({"cmd":"approve", "session":"worker", "decision":"allow"}));
                            assert!(answer.ok, "{answer:?}");
                            assert_eq!(answer.result["command"], "touch live");
                        }
                        _ => unreachable!(),
                    }
                    assert_eq!(app.runtime.resolution("dead"), Some(Disposition::Expired), "{reader}");
                    if matches!(reader, "poll" | "approvable") {
                        assert_eq!(app.runtime.sessions[0].approvals[0].id, "live");
                        app.runtime.record_events();
                        let event = app
                            .runtime
                            .events
                            .iter()
                            .find(|event| event.seq > cursor)
                            .expect("the chief hears the new head");
                        assert_eq!(event.approval.as_ref().unwrap()["command"], "touch live");
                        app.runtime.approve(0, "live", Decision::Allow, None).unwrap();
                    }
                    assert!(live.wait(Duration::ZERO).stdout.unwrap().contains("allow"), "{reader}");
                    assert!(app.runtime.sessions[0].approvals.is_empty());
                    assert_eq!(app.runtime.resolution("live"), Some(Disposition::Allowed));
                    let projections = atomics_core::projection::Projections::default();
                    app.runtime.publish_projections(
                        &projections,
                        &mut super::super::projection::DiffSummaries::default(),
                        &super::super::projection::DiffBodies::default(),
                    );
                    assert_eq!(projections.resolution("dead"), Some(Disposition::Expired));
                }
            },
        );
    }

    #[test]
    fn approving_only_dead_hooks_clears_the_queue_and_reports_no_pending_request() {
        with_app(
            "app::approvals::tests::approving_only_dead_hooks_clears_the_queue_and_reports_no_pending_request",
            |app, _| {
                for control in [false, true] {
                    app.runtime.sessions =
                        vec![session("worker", AgentKind::Generic, AgentStatus::NeedsInput, &app.runtime.launch_dir)];
                    app.runtime.sessions[0].approval_hold = true;
                    for id in ["dead-one", "dead-two"] {
                        hook_request(app, id).wait(Duration::ZERO);
                    }
                    let now = Instant::now();
                    app.runtime.sessions[0].menu_absent_since = Some(now - APPROVAL_MENU_GRACE);
                    app.runtime.approvals_checked = now - APPROVAL_CHECK_INTERVAL;
                    let error = if control {
                        answer_pending(app, json!({"cmd":"approve", "session":"worker", "decision":"allow"}))
                            .error
                            .unwrap()
                    } else {
                        app.runtime.approve(0, "dead-one", Decision::Allow, None).unwrap_err().message
                    };
                    assert_eq!(error, "no pending permission request");
                    assert!(app.runtime.sessions[0].approvals.is_empty());
                    assert!(!app.runtime.sessions[0].approval_hold);
                    assert_eq!(app.runtime.sessions[0].status, AgentStatus::Working);
                    for id in ["dead-one", "dead-two"] {
                        assert_eq!(app.runtime.resolution(id), Some(Disposition::Expired));
                    }
                }
            },
        );
    }

    #[test]
    fn a_failed_approval_button_displays_the_decision_error() {
        with_app("app::approvals::tests::a_failed_approval_button_displays_the_decision_error", |app, ctx| {
            app.runtime.sessions =
                vec![session("worker", AgentKind::Generic, AgentStatus::NeedsInput, &app.runtime.launch_dir)];
            let reply = hook_request(app, "reviewed");
            let mut time = 0.0;
            let mut frame = |app: &mut Desktop<'_>, events| {
                time += 0.05;
                let output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1400.0, 900.0))),
                        time: Some(time),
                        events,
                        ..Default::default()
                    },
                    |ui| {
                        egui::CentralPanel::default().show(ui, |ui| {
                            app.draw(|backend, client| atomics_ui::topbar::notice(ui, backend, client));
                            let layout = layout::compute(1400.0, 900.0, 1, 0, false, 0.0, &app.runtime.config.layout);
                            app.tiles(ui, ui.max_rect(), &layout, &[0]);
                        });
                    },
                );
                let text: Vec<_> = output
                    .shapes
                    .iter()
                    .filter_map(|shape| match &shape.shape {
                        egui::epaint::Shape::Text(t) => {
                            Some((t.galley.text().to_string(), t.pos + t.galley.rect.center().to_vec2()))
                        }
                        _ => None,
                    })
                    .collect();
                output.drop_without_applying_deltas();
                text
            };
            let pos = frame(app, vec![]).into_iter().find(|(text, _)| text == "Allow").unwrap().1;
            for pressed in [true, false] {
                frame(
                    app,
                    vec![
                        egui::Event::PointerMoved(pos),
                        egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
            assert!(!app.client.approval_decisions.is_empty(), "the button posted a decision");
            reply.wait(Duration::ZERO);
            app.runtime.drain_actions();
            let error = "the permission hook has returned; answer in the agent terminal";
            assert_eq!(app.runtime.toast.as_ref().unwrap().0, error);
            app.apply_client_events(ctx);
            assert!(app.client.approval_decisions.is_empty());
            assert!(frame(app, vec![]).iter().any(|(text, _)| text.contains(error)), "the error must be drawn");
        });
    }

    #[test]
    fn recorded_native_menus_are_the_one_pending_approval_for_approve_send_and_tiles() {
        with_app(
            "app::approvals::tests::recorded_native_menus_are_the_one_pending_approval_for_approve_send_and_tiles",
            |app, _ctx| {
                let codex = "atomics queue --class test -- ./check.sh";
                let claude = "Create the fixture file the test expects\ntouch fixture";
                for (screen, kind, command, decision, keys) in [
                    ("codex-0.165-approval-screen.txt", "codex", codex, Decision::Allow, "31 0d"),
                    ("codex-0.165-approval-screen.txt", "codex", codex, Decision::Deny, "1b"),
                    ("claude-2.1-approval-screen.txt", "claude", claude, Decision::Allow, "0d"),
                    ("claude-2.1-approval-screen.txt", "claude", claude, Decision::Deny, "1b"),
                ] {
                    let terminal = menu_agent(app, &[&recorded(screen)]);
                    // An agent without hooks whose turn ended on its own menu: no hook request exists.
                    let mut worker = session("worker", AgentKind::Generic, AgentStatus::Done, &app.runtime.launch_dir);
                    worker.terminal = Some(terminal);
                    app.runtime.sessions = vec![worker];
                    app.runtime.busy_checked = Instant::now() - Duration::from_secs(1);
                    app.runtime.track_busy();
                    let worker = &app.runtime.sessions[0];
                    assert_eq!(worker.status, AgentStatus::NeedsInput, "{kind}");
                    assert_eq!(worker.approvals.len(), 1, "{kind}");
                    assert!(worker.approval_hold);
                    let id = worker.approvals[0].id.clone();
                    assert_eq!(worker.approvals[0].request.command.as_deref(), Some(command));

                    // The tile's banner, its session row, ctl and every protocol client show that one request.
                    let backend = app.runtime.backend();
                    assert_eq!(backend.session_approval(0).map(|row| row.id), Some(id.clone()));
                    assert_eq!(backend.session_row(0).unwrap().pending_approvals, 1);
                    let projections = atomics_core::projection::Projections::default();
                    app.runtime.publish_projections(
                        &projections,
                        &mut super::super::projection::DiffSummaries::default(),
                        &super::super::projection::DiffBodies::default(),
                    );
                    let loaded = projections.load();
                    assert_eq!(loaded.approvals.data.len(), 1);
                    assert_eq!(loaded.approvals.data[0].id, id);
                    assert_eq!(loaded.approvals.data[0].command.as_deref(), Some(command));
                    assert_eq!(loaded.sessions.data[0].pending_approvals, 1);
                    let listed = request(app, json!({"cmd":"list"})).result;
                    assert_eq!(listed["sessions"][0]["approval"]["command"], command);
                    assert_eq!(listed["sessions"][0]["approval_menu"], kind);

                    // Polling keeps its ID; raw keys wait for atomics_approve, which can answer it.
                    poll_after_quiet(app);
                    assert_eq!(app.runtime.sessions[0].approvals.iter().map(|p| &p.id).collect::<Vec<_>>(), [&id]);
                    let raw = request(app, json!({"cmd":"send", "session":"worker", "raw":true, "text":"y"}));
                    assert!(raw.error.unwrap().contains("atomics_approve"), "{kind}");

                    let answered =
                        answer_pending(app, json!({"cmd":"approve", "session":"worker", "decision":decision}));
                    assert!(answered.ok, "{kind} {decision:?}: {answered:?}");
                    assert_eq!(answered.result["command"], command);
                    assert_eq!(answered.result["menu"], kind);
                    assert_eq!(answered.result["tool"], "Bash");
                    shows(app.runtime.sessions[0].terminal.as_ref().unwrap(), &format!("Keys: {keys}\n"));
                    assert_eq!(app.runtime.resolution(&id), Some(disposition(decision)));
                    assert!(app.runtime.sessions[0].allowed.is_empty(), "a menu answers only itself");

                    // The menu is gone: nothing waits, the tile has no banner and raw keys reach the agent.
                    poll_after_quiet(app);
                    assert!(app.runtime.sessions[0].approvals.is_empty());
                    assert!(!app.runtime.sessions[0].approval_hold);
                    assert!(app.runtime.backend().session_approval(0).is_none());
                    let raw = request(app, json!({"cmd":"send", "session":"worker", "raw":true, "text":"z"}));
                    assert!(raw.ok, "{raw:?}");
                    shows(app.runtime.sessions[0].terminal.as_ref().unwrap(), &format!("Keys: {keys} 7a"));
                    let again = answer_pending(app, json!({"cmd":"approve", "session":"worker", "decision":"allow"}));
                    assert_eq!(again.error.as_deref(), Some("no pending permission request"));
                    app.runtime.sessions.clear();
                }
            },
        );
    }

    #[test]
    fn menus_that_are_not_command_approvals_take_raw_keys() {
        with_app("app::approvals::tests::menus_that_are_not_command_approvals_take_raw_keys", |app, _ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            let edits = dir.path().join("edits.txt");
            std::fs::write(
                &edits,
                "Allow these edits?\n› 1. Yes, proceed\n  2. No\nPress enter to confirm or esc to cancel",
            )
            .unwrap();
            let terminal = menu_agent(app, &[&edits]);
            let mut worker = session("worker", AgentKind::Generic, AgentStatus::Done, &app.runtime.launch_dir);
            worker.terminal = Some(terminal);
            app.runtime.sessions = vec![worker];
            app.runtime.busy_checked = Instant::now() - Duration::from_secs(1);
            app.runtime.track_busy();
            assert_eq!(app.runtime.sessions[0].status, AgentStatus::NeedsInput);
            // Before any poll holds input for it, atomics_approve already reads the menu on screen.
            app.runtime.needs_input_checked = Instant::now();
            let approve = answer_pending(app, json!({"cmd":"approve", "session":"worker", "decision":"allow"}));
            assert!(approve.error.unwrap().contains("atomics_send raw"));
            app.runtime.needs_input_checked = Instant::now() - NEEDS_INPUT_CHECK_INTERVAL;
            poll_after_quiet(app);
            // The agent waits on a menu, and typed messages still wait for it...
            assert_eq!(app.runtime.sessions[0].status, AgentStatus::NeedsInput);
            assert!(app.runtime.sessions[0].approval_hold);
            assert!(app.runtime.sessions[0].approvals.is_empty());
            assert_eq!(app.runtime.deliver(0, "steer later"), Delivery::Held);
            // ...but atomics_approve cannot answer it, so the keys that do are not refused.
            let approve = answer_pending(app, json!({"cmd":"approve", "session":"worker", "decision":"allow"}));
            assert!(approve.error.unwrap().contains("atomics_send raw"));
            let raw = request(app, json!({"cmd":"send", "session":"worker", "raw":true, "text":"1"}));
            assert!(raw.ok, "{raw:?}");
            shows(app.runtime.sessions[0].terminal.as_ref().unwrap(), "Keys: 31\n");
            app.runtime.sessions.clear();
        });
    }

    #[test]
    fn a_hidden_native_command_is_shown_before_it_is_approved_and_a_hook_replaces_the_screen() {
        with_app(
            "app::approvals::tests::a_hidden_native_command_is_shown_before_it_is_approved_and_a_hook_replaces_the_screen",
            |app, _ctx| {
                let dir = crate::test_support::tempdir().unwrap();
                let hidden = dir.path().join("hidden.txt");
                std::fs::write(&hidden, "Would you like to run the following command?\n\nReason: A long explanation of why access is needed\n  [… 9 lines] ⌃a view all\n\n› 1. Yes, proceed (y)\n  2. No, and tell Codex what to do differently (esc)\nPress enter to confirm or esc to cancel").unwrap();
                let terminal = menu_agent(app, &[&hidden, &recorded("codex-0.165-approval-screen.txt")]);
                let mut worker = session("worker", AgentKind::Generic, AgentStatus::Done, &app.runtime.launch_dir);
                worker.terminal = Some(terminal);
                app.runtime.sessions = vec![worker];
                app.runtime.busy_checked = Instant::now() - Duration::from_secs(1);
                app.runtime.track_busy();
                let first = app.runtime.sessions[0].approvals[0].id.clone();
                assert_eq!(app.runtime.sessions[0].approvals[0].request.command, None);
                assert_eq!(
                    app.runtime.sessions[0].approvals[0].request.reason.as_deref(),
                    Some("A long explanation of why access is needed")
                );
                let expanded = answer_pending(app, json!({"cmd":"approve", "session":"worker", "decision":"allow"}));
                assert!(expanded.error.unwrap().contains("expanded"));
                shows(app.runtime.sessions[0].terminal.as_ref().unwrap(), "$ atomics queue");
                // The command now shows: a new request under a new ID, and the one decided unseen is gone.
                poll_after_quiet(app);
                let second = app.runtime.sessions[0].approvals[0].id.clone();
                assert_ne!(first, second);
                assert_eq!(
                    app.runtime.sessions[0].approvals[0].request.command.as_deref(),
                    Some("atomics queue --class test -- ./check.sh")
                );
                let stale = app.runtime.decide(&first, Decision::Allow, None).unwrap_err();
                assert_eq!(stale.code, ErrorCode::NotFound);

                // A hook request is the request itself: the screen's stand-in gives way to it.
                let reply = Arc::new(ipc::ApprovalReply::default());
                let event = atomics_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name":"PermissionRequest", "session_id":"worker", "tool_name":"Bash",
                    "tool_input":{"command":"atomics queue --class test -- ./check.sh"}
                }))
                .unwrap();
                app.runtime.on_hook(HookMessage {
                    pane: Some("worker".into()),
                    ancestors: vec![],
                    event,
                    delivered: 0,
                    approval_reply: Some(reply.clone()),
                });
                assert_eq!(app.runtime.sessions[0].approvals.len(), 1);
                assert!(app.runtime.sessions[0].approvals[0].native.is_none());
                assert_eq!(app.runtime.decide(&second, Decision::Allow, None).unwrap_err().code, ErrorCode::NotFound);
                let answered = answer_pending(app, json!({"cmd":"approve", "session":"worker", "decision":"allow"}));
                assert_eq!(answered.result["menu"], serde_json::Value::Null);
                assert!(reply.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
                app.runtime.sessions.clear();
            },
        );
    }

    #[test]
    fn approval_errors_keep_their_specific_guidance() {
        with_app("app::approvals::tests::approval_errors_keep_their_specific_guidance", |app, _| {
            for (tool, terminal, expected) in [
                ("Azure REST", false, "the Azure request expired; retry the request"),
                ("Bash", false, "the permission hook has returned; answer in the agent terminal"),
                ("Bash", true, "the permission menu is not visible; retry or answer in the terminal"),
            ] {
                let mut worker =
                    session("worker", AgentKind::Generic, AgentStatus::NeedsInput, &app.runtime.launch_dir);
                if terminal {
                    worker.terminal = Some(
                        Terminal::spawn(
                            app.runtime.wake.clone(),
                            app.runtime.theme.terminal_colors(),
                            crate::test_support::program("cat"),
                            vec![],
                            &app.runtime.launch_dir,
                            HashMap::new(),
                            100,
                        )
                        .unwrap(),
                    );
                }
                worker.approvals.push(PendingApproval {
                    id: "pending".into(),
                    request: ApprovalRequest {
                        tool: tool.into(),
                        command: Some("touch fixture".into()),
                        cwd: app.runtime.launch_dir.clone(),
                        reason: None,
                        input: json!({}),
                        warning: None,
                    },
                    reply: None,
                    native: None,
                    stash: Default::default(),
                });
                app.runtime.sessions = vec![worker];
                app.runtime.approvals_checked = Instant::now();
                let error = app.runtime.decide("pending", Decision::Allow, None).unwrap_err();
                assert_eq!(error.message, expected);
            }
            app.runtime.sessions.clear();
        });
    }

    #[test]
    fn azure_writes_from_sandboxes_use_the_permission_banner_and_never_persist_allow_always() {
        with_app(
            "app::approvals::tests::azure_writes_from_sandboxes_use_the_permission_banner_and_never_persist_allow_always",
            |app, ctx| {
                let dir = crate::test_support::tempdir().unwrap();
                let upstream = crate::relay::fake::start();
                let config = atomics_core::isolation::AzureConfig { enabled: true, ..Default::default() };
                app.runtime.config.isolation.sandbox.azure = config.clone();
                super::super::tests::persist_config(app.runtime);
                let azure =
                    crate::relay::azure::Azure::new(crate::relay::azure::tests::fake(dir.path()), config.clone());
                app.runtime.relay = Arc::new(
                    crate::relay::Service::with_upstreams(
                        dir.path().to_path_buf(),
                        crate::relay::Keychain::at(Path::new("/no-keychain"), false),
                        [("management.azure.com".into(), upstream.url)].into(),
                    )
                    .with_azure(azure),
                );
                let granted = app
                    .runtime
                    .relay
                    .grant_session("atomics-worker", false, &config)
                    .unwrap()
                    .expect("Azure-enabled approval fixture must receive a relay grant");
                app.runtime.apply_config();
                app.runtime.sessions =
                    vec![session("worker", AgentKind::Generic, AgentStatus::Working, &app.runtime.launch_dir)];
                app.runtime.sessions[0].sandbox = Some("atomics-worker".into());
                for decision in [Decision::Deny, Decision::AllowAlways] {
                    let port = granted.port;
                    let placeholder = granted.azure.as_ref().expect("Azure approval fixture must have a placeholder");
                    let request = format!(
                        "PUT /management.azure.com/v1/models?api-version=2026 HTTP/1.1\r\nauthorization: Bearer {placeholder}\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{{}}"
                    );
                    let client = std::thread::spawn(move || crate::relay::fake::send(port, &request).0);
                    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                    while app.runtime.sessions[0].approvals.is_empty() {
                        assert!(Instant::now() < deadline);
                        app.runtime.expire_approvals(Instant::now());
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    assert_eq!(app.runtime.sessions[0].status, AgentStatus::NeedsInput);
                    let pending = &app.runtime.sessions[0].approvals[0];
                    let id = pending.id.clone();
                    assert_eq!(pending.request.tool, "Azure REST");
                    assert!(pending.request.command.as_ref().unwrap().contains("api-version=2026"));
                    // Normal sandbox hook permissions are ignored, but a live relay approval must survive polling.
                    app.runtime.expire_approvals(Instant::now() + Duration::from_secs(1));
                    assert_eq!(app.runtime.sessions[0].approvals.len(), 1);
                    let output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1400.0, 900.0))),
                            ..Default::default()
                        },
                        |ui| {
                            egui::CentralPanel::default().show(ui, |ui| {
                                let layout =
                                    layout::compute(1400.0, 900.0, 1, 0, false, 0.0, &app.runtime.config.layout);
                                app.tiles(ui, ui.max_rect(), &layout, &[0]);
                            });
                        },
                    );
                    let text: Vec<_> = output
                        .shapes
                        .iter()
                        .filter_map(|shape| match &shape.shape {
                            egui::epaint::Shape::Text(text) => Some(text.galley.text().to_string()),
                            _ => None,
                        })
                        .collect();
                    for label in ["Permission requested", "Allow", "Deny"] {
                        assert!(text.iter().any(|text| text == label), "{text:?}");
                    }
                    assert!(!text.iter().any(|text| text == "Allow always"));
                    output.drop_without_applying_deltas();
                    app.runtime.approve(0, &id, decision, None).unwrap();
                    let expected = if decision == Decision::Deny { 403 } else { 200 };
                    assert!(client.join().unwrap().starts_with(&format!("HTTP/1.1 {expected}")));
                    assert!(app.runtime.sessions[0].allowed.is_empty());
                    assert_eq!(app.runtime.sessions[0].status, AgentStatus::Working);
                }
                app.runtime.relay.revoke(&granted);
            },
        );
    }

    #[test]
    fn delayed_duplicate_decisions_cannot_approve_the_next_request() {
        with_app("app::approvals::tests::delayed_duplicate_decisions_cannot_approve_the_next_request", |app, ctx| {
            app.runtime.sessions =
                vec![session("worker", AgentKind::Generic, AgentStatus::NeedsInput, &app.runtime.launch_dir)];
            let reviewed = Arc::new(ipc::ApprovalReply::default());
            let unreviewed = Arc::new(ipc::ApprovalReply::default());
            for (id, reply) in [("reviewed", reviewed.clone()), ("unreviewed", unreviewed.clone())] {
                app.runtime.sessions[0].approvals.push(PendingApproval {
                    id: id.into(),
                    request: ApprovalRequest {
                        tool: "Bash".into(),
                        command: Some(format!("touch {id}")),
                        cwd: app.runtime.launch_dir.clone(),
                        reason: None,
                        input: json!({}),
                        warning: None,
                    },
                    reply: Some(reply),
                    native: None,
                    stash: Default::default(),
                });
            }
            let mut time = 0.0;
            // Deliberately leave host actions AND client acknowledgements undrained.
            let mut frame = |app: &mut Desktop<'_>, events| {
                time += 0.05;
                let output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1400.0, 900.0))),
                        time: Some(time),
                        events,
                        ..Default::default()
                    },
                    |ui| {
                        egui::CentralPanel::default().show(ui, |ui| {
                            let layout = layout::compute(1400.0, 900.0, 1, 0, false, 0.0, &app.runtime.config.layout);
                            app.tiles(ui, ui.max_rect(), &layout, &[0]);
                        });
                    },
                );
                let text: Vec<_> = output
                    .shapes
                    .iter()
                    .filter_map(|shape| match &shape.shape {
                        egui::epaint::Shape::Text(t) => {
                            Some((t.galley.text().to_string(), t.pos + t.galley.rect.center().to_vec2()))
                        }
                        _ => None,
                    })
                    .collect();
                output.drop_without_applying_deltas();
                text
            };
            let pos = frame(app, vec![]).into_iter().find(|(text, _)| text == "Allow").unwrap().1;
            for _ in 0..2 {
                for pressed in [true, false] {
                    frame(
                        app,
                        vec![
                            egui::Event::PointerMoved(pos),
                            egui::Event::PointerButton {
                                pos,
                                button: egui::PointerButton::Primary,
                                pressed,
                                modifiers: egui::Modifiers::NONE,
                            },
                        ],
                    );
                }
            }
            let text = frame(app, vec![]);
            assert!(text.iter().any(|(text, _)| text == "Decision pending…"));
            assert!(text.iter().any(|(text, _)| text == "touch reviewed"));
            for label in ["Allow", "Allow always", "Deny"] {
                assert_eq!(
                    ctx.data(|data| data.get_temp::<bool>(egui::Id::unique(("approval-button", label)))),
                    Some(false)
                );
            }
            assert_eq!(app.runtime.sessions[0].approvals.len(), 2);
            // Even a duplicate already in transit must be rejected by the host.
            let (tx, rx) = std::sync::mpsc::channel();
            app.post(move |state| {
                tx.send(state.approve(0, "reviewed", Decision::Allow, None)).unwrap();
            });
            app.runtime.drain_actions();
            assert!(rx.recv().unwrap().unwrap_err().message.contains("stale"));
            assert!(reviewed.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
            assert_eq!(app.runtime.sessions[0].approvals.len(), 1);
            assert_eq!(app.runtime.sessions[0].approvals[0].id, "unreviewed");
            assert!(unreviewed.answer(HookReply::default()), "the unseen request was already decided");
            app.apply_client_events(ctx);
            frame(app, vec![]);
            assert!(app.client.approval_decisions.is_empty());
            assert_eq!(
                ctx.data(|data| data.get_temp::<bool>(egui::Id::unique(("approval-button", "Allow")))),
                Some(true)
            );
        });
    }

    #[test]
    fn permission_banner_buttons_answer_hooks_and_chief_liveness_selects_the_wait() {
        with_app(
            "app::approvals::tests::permission_banner_buttons_answer_hooks_and_chief_liveness_selects_the_wait",
            |app, ctx| {
                let terminal = Terminal::spawn(
                    app.runtime.wake.clone(),
                    app.runtime.theme.terminal_colors(),
                    crate::test_support::program("sleep"),
                    vec!["30".into()],
                    &app.runtime.launch_dir,
                    HashMap::new(),
                    100,
                )
                .unwrap();
                app.runtime.sessions =
                    vec![session("worker", AgentKind::Generic, AgentStatus::NeedsInput, &app.runtime.launch_dir)];
                app.runtime.sessions[0].terminal = Some(terminal);
                app.runtime.record_events();
                assert!(!app.runtime.hub.lock().unwrap().chief_running);
                let mut time = 0.0;
                let mut frame = |app: &mut Desktop<'_>, events| {
                    time += 0.05;
                    let output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1400.0, 900.0))),
                            time: Some(time),
                            events,
                            ..Default::default()
                        },
                        |ui| {
                            egui::CentralPanel::default().show(ui, |ui| {
                                let layout =
                                    layout::compute(1400.0, 900.0, 1, 0, false, 0.0, &app.runtime.config.layout);
                                app.tiles(ui, ui.max_rect(), &layout, &[0]);
                            });
                        },
                    );
                    app.runtime.drain_actions();
                    let text: Vec<_> = output
                        .shapes
                        .iter()
                        .filter_map(|shape| match &shape.shape {
                            egui::epaint::Shape::Text(t) => {
                                Some((t.galley.text().to_string(), t.pos + t.galley.rect.center().to_vec2()))
                            }
                            _ => None,
                        })
                        .collect();
                    output.drop_without_applying_deltas();
                    text
                };
                for (label, decision) in
                    [("Allow", Decision::Allow), ("Allow always", Decision::AllowAlways), ("Deny", Decision::Deny)]
                {
                    let reply = Arc::new(ipc::ApprovalReply::default());
                    app.runtime.sessions[0].status = AgentStatus::NeedsInput;
                    app.runtime.sessions[0].approval_hold = true;
                    app.runtime.sessions[0].approvals.push(PendingApproval {
                        id: uuid::Uuid::new_v4().to_string(),
                        request: ApprovalRequest {
                            tool: "Bash".into(),
                            command: Some("touch banner-fixture".into()),
                            cwd: app.runtime.launch_dir.clone(),
                            reason: Some("Verify permission controls".into()),
                            input: json!({}),
                            warning: None,
                        },
                        reply: Some(reply.clone()),
                        native: None,
                        stash: Default::default(),
                    });
                    let text = frame(app, vec![]);
                    for expected in ["Permission requested", "touch banner-fixture", "Verify permission controls"] {
                        assert!(text.iter().any(|(text, _)| text == expected), "{expected}");
                    }
                    assert!(text.iter().any(|(text, _)| text.starts_with("in ")), "the folder it runs in");
                    let pos = text.iter().find(|(text, _)| text == label).unwrap().1;
                    for pressed in [true, false] {
                        frame(
                            app,
                            vec![
                                egui::Event::PointerMoved(pos),
                                egui::Event::PointerButton {
                                    pos,
                                    button: egui::PointerButton::Primary,
                                    pressed,
                                    modifiers: egui::Modifiers::NONE,
                                },
                            ],
                        );
                    }
                    let result: serde_json::Value =
                        serde_json::from_str(&reply.wait(Duration::ZERO).stdout.unwrap()).unwrap();
                    assert_eq!(
                        result["hookSpecificOutput"]["decision"]["behavior"],
                        if decision == Decision::Deny { "deny" } else { "allow" }
                    );
                    assert!(app.runtime.sessions[0].approvals.is_empty());
                    assert_eq!(app.runtime.sessions[0].status, AgentStatus::Working);
                    assert!(!frame(app, vec![]).iter().any(|(text, _)| text == "Permission requested"));
                }
                assert_eq!(app.runtime.sessions[0].allowed.len(), 1);
                app.runtime.sessions[0].chief = true;
                app.runtime.record_events();
                assert!(app.runtime.hub.lock().unwrap().chief_running);
                app.runtime.sessions[0].status = AgentStatus::Exited;
                app.runtime.record_events();
                assert!(!app.runtime.hub.lock().unwrap().chief_running);
                app.runtime.sessions[0].status = AgentStatus::Working;
                app.runtime.record_events();
                app.runtime.close("worker");
                assert!(!app.runtime.hub.lock().unwrap().chief_running);
            },
        );
    }

    #[test]
    fn hidden_permission_commands_expand_before_approval_and_running_clears_needs_input() {
        with_app(
            "app::approvals::tests::hidden_permission_commands_expand_before_approval_and_running_clears_needs_input",
            |app, _ctx| {
                let fixture = crate::app::tests::fixture_agent("codex");
                app.runtime.config.agents.push(atomics_core::config::AgentConfig {
                    name: "test".into(),
                    command: vec![fixture.to_string_lossy().into_owned()],
                    kind: AgentKind::Generic,
                    busy_text: Some("esc to interrupt".into()),
                    isolation: None,
                });
                let terminal = Terminal::spawn(
                    app.runtime.wake.clone(),
                    app.runtime.theme.terminal_colors(),
                    fixture.to_string_lossy().into_owned(),
                    vec!["--mode".into(), "hidden".into()],
                    &app.runtime.launch_dir,
                    HashMap::new(),
                    100,
                )
                .unwrap();
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
                while !terminal.screen_text(60).contains("view all") {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                app.runtime.sessions =
                    vec![session("hidden", AgentKind::Generic, AgentStatus::NeedsInput, &app.runtime.launch_dir)];
                app.runtime.sessions[0].terminal = Some(terminal);
                app.runtime.sessions[0].hooks_active = true;
                let reply = Arc::new(ipc::ApprovalReply::default());
                let permission = atomics_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name":"PermissionRequest", "session_id":"hidden", "tool_name":"Bash",
                    "tool_input":{"command":"touch fixture"}
                }))
                .unwrap();
                app.runtime.sessions[0].approval_hold = true;
                app.runtime.sessions[0].approvals.push(PendingApproval {
                    id: uuid::Uuid::new_v4().to_string(),
                    request: ApprovalRequest::from_event(&permission),
                    reply: Some(reply.clone()),
                    native: None,
                    stash: Default::default(),
                });
                // The hook has returned before its hidden fallback menu can be answered.
                assert!(reply.wait(Duration::ZERO).stdout.is_none());
                assert!(
                    app.runtime
                        .approve(
                            0,
                            &app.runtime.sessions[0]
                                .approvals
                                .first()
                                .map(|pending| pending.id.clone())
                                .unwrap_or_default(),
                            Decision::Allow,
                            None
                        )
                        .unwrap_err()
                        .message
                        .contains("expanded")
                );
                assert_eq!(app.runtime.sessions[0].approvals.len(), 1);
                while !app.runtime.sessions[0].terminal.as_ref().unwrap().screen_text(60).contains("$ touch fixture") {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                app.runtime
                    .approve(
                        0,
                        &app.runtime.sessions[0]
                            .approvals
                            .first()
                            .map(|pending| pending.id.clone())
                            .unwrap_or_default(),
                        Decision::Allow,
                        None,
                    )
                    .unwrap();
                while !app.runtime.sessions[0].terminal.as_ref().unwrap().screen_text(60).contains("Approved") {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                app.runtime.sessions[0].approval_hold = false;
                app.runtime.sessions[0].status = AgentStatus::NeedsInput;
                app.runtime.sessions[0].menu_absent_since = Some(Instant::now() - APPROVAL_MENU_GRACE);
                app.runtime.approvals_checked = Instant::now() - APPROVAL_CHECK_INTERVAL;
                app.runtime.needs_input_checked = Instant::now() - NEEDS_INPUT_CHECK_INTERVAL;
                app.runtime.expire_approvals(Instant::now());
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::Working);
                assert!(app.runtime.sessions[0].approvals.is_empty());
                assert!(!app.runtime.sessions[0].approval_hold);
                let reply = Arc::new(ipc::ApprovalReply::default());
                app.runtime.sessions[0].approvals.push(PendingApproval {
                    id: uuid::Uuid::new_v4().to_string(),
                    request: ApprovalRequest::from_event(&permission),
                    reply: Some(reply.clone()),
                    native: None,
                    stash: Default::default(),
                });
                app.runtime.sessions[0].status = AgentStatus::NeedsInput;
                let event = atomics_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name":"PreToolUse", "session_id":"hidden", "tool_name":"Bash",
                    "tool_input":{"command":"touch fixture"}
                }))
                .unwrap();
                app.runtime.on_hook(HookMessage {
                    pane: None,
                    ancestors: vec![],
                    event,
                    delivered: 0,
                    approval_reply: None,
                });
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::Working);
                assert!(app.runtime.sessions[0].approvals.is_empty());
                assert!(reply.wait(Duration::ZERO).stdout.is_none());
            },
        );
    }

    #[test]
    fn expired_hooks_wait_for_menu_quiet_or_lifecycle() {
        with_app("app::approvals::tests::expired_hooks_wait_for_menu_quiet_or_lifecycle", |app, _ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            app.runtime.sessions = vec![session("stale", AgentKind::Generic, AgentStatus::NeedsInput, dir.path())];
            let permission = atomics_core::hook::HookEvent::from_payload(&json!({
                "hook_event_name":"PermissionRequest", "session_id":"stale", "cwd":dir.path(),
                "tool_name":"Bash", "tool_input":{"command":"touch fixture", "description":"test"}
            }))
            .unwrap();
            let reply = Arc::new(ipc::ApprovalReply::default());
            app.runtime.sessions[0].approval_hold = true;
            app.runtime.sessions[0].approvals.push(PendingApproval {
                id: uuid::Uuid::new_v4().to_string(),
                request: ApprovalRequest::from_event(&permission),
                reply: Some(reply.clone()),
                native: None,
                stash: Default::default(),
            });
            let raw = request(app, json!({"cmd":"send", "session":"stale", "raw":true, "text":"y"}));
            assert!(!raw.ok);
            assert!(raw.error.unwrap().contains("atomics_approve"));
            assert_eq!(app.runtime.deliver(0, "held plain message"), Delivery::Held);
            let now = Instant::now();
            app.runtime.approvals_checked = now - APPROVAL_CHECK_INTERVAL;
            app.runtime.expire_approvals(now);
            assert_eq!(app.runtime.sessions[0].approvals.len(), 1);
            reply.wait(Duration::ZERO);
            let first_check = now + APPROVAL_CHECK_INTERVAL;
            app.runtime.expire_approvals(first_check);
            assert_eq!(app.runtime.sessions[0].approvals.len(), 1, "an expired hook may precede its menu");
            assert_eq!(app.runtime.sessions[0].held, ["held plain message"]);
            let second_check = first_check + APPROVAL_CHECK_INTERVAL;
            app.runtime.expire_approvals(second_check);
            assert_eq!(app.runtime.sessions[0].approvals.len(), 1, "the menu quiet period must elapse");
            app.runtime.sessions[0].menu_absent_since = Some(first_check - APPROVAL_MENU_GRACE);
            app.runtime.expire_approvals(second_check);
            assert_eq!(app.runtime.sessions[0].approvals.len(), 1, "screen checks must be throttled");
            app.runtime.expire_approvals(second_check + APPROVAL_CHECK_INTERVAL);
            assert!(app.runtime.sessions[0].approvals.is_empty());
            assert!(app.runtime.sessions[0].held.is_empty());
            assert_eq!(app.runtime.hub.lock().unwrap().outbox["stale"], ["held plain message"]);
            let menu = "Allow these file edits?\n› 1. Yes, proceed\n  2. No\nPress enter to confirm or esc to cancel";
            assert!(atomics_core::approvals::menu_key(menu, Some("touch fixture"), Decision::Allow).is_none());
            let terminal = Terminal::spawn(
                app.runtime.wake.clone(),
                app.runtime.theme.terminal_colors(),
                "/bin/sh".into(),
                vec!["-c".into(), "printf '%s' \"$1\"; exec sleep 10".into(), "fixture".into(), menu.into()],
                dir.path(),
                HashMap::new(),
                100,
            )
            .unwrap();
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(2));
            while !terminal.screen_text(60).contains("esc to cancel") {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            app.runtime.sessions[0].terminal = Some(terminal);
            app.runtime.sessions[0].status = AgentStatus::NeedsInput;
            app.runtime.sessions[0].approval_hold = true;
            app.runtime.sessions[0].menu_absent_since = Some(Instant::now() - APPROVAL_MENU_GRACE);
            app.runtime.sessions[0].approvals.push(PendingApproval {
                id: uuid::Uuid::new_v4().to_string(),
                request: ApprovalRequest::from_event(&permission),
                reply: Some(reply),
                native: None,
                stash: Default::default(),
            });
            assert_eq!(app.runtime.deliver(0, "held while editing"), Delivery::Held);
            let before = app.runtime.sessions[0].terminal.as_ref().unwrap().screen_text(60);
            app.runtime.approvals_checked = Instant::now() - APPROVAL_CHECK_INTERVAL;
            app.runtime.expire_approvals(Instant::now());
            assert!(app.runtime.sessions[0].approvals.is_empty(), "an edit menu cannot keep a dead command request");
            assert!(app.runtime.sessions[0].menu_absent_since.is_none(), "a menu drawn late resets the quiet period");
            assert_eq!(app.runtime.sessions[0].held, ["held while editing"]);
            let event = atomics_core::hook::HookEvent::from_payload(&json!({"hook_event_name":"PreToolUse", "session_id":"stale", "cwd":dir.path(), "tool_name":"Bash", "tool_input":{"command":"cat fixture"}})).unwrap();
            app.runtime.on_hook(HookMessage {
                pane: None,
                ancestors: vec![],
                event,
                delivered: 0,
                approval_reply: None,
            });
            assert!(app.runtime.sessions[0].approvals.is_empty());
            assert_eq!(app.runtime.sessions[0].status, AgentStatus::Working);
            assert!(app.runtime.sessions[0].approval_hold, "PreToolUse alone must not release input into a menu");
            assert_eq!(app.runtime.deliver(0, "still held"), Delivery::Held);
            app.runtime.approvals_checked = Instant::now() - APPROVAL_CHECK_INTERVAL;
            app.runtime.expire_approvals(Instant::now());
            assert_eq!(app.runtime.sessions[0].held.len(), 2);
            assert_eq!(app.runtime.sessions[0].terminal.as_ref().unwrap().screen_text(60), before);
            app.runtime.sessions[0].terminal = None;
            let event = atomics_core::hook::HookEvent::from_payload(
                &json!({"hook_event_name":"Interrupt", "session_id":"stale", "cwd":dir.path()}),
            )
            .unwrap();
            app.runtime.on_hook(HookMessage {
                pane: None,
                ancestors: vec![],
                event,
                delivered: 0,
                approval_reply: None,
            });
            assert!(!app.runtime.sessions[0].approval_hold);
            assert!(app.runtime.sessions[0].held.is_empty());
            assert!(
                app.runtime.hub.lock().unwrap().outbox["stale"]
                    .last()
                    .unwrap()
                    .contains("held while editing\n\nstill held")
            );
            app.runtime.sessions.clear();
        });
    }

    #[test]
    fn approvals_and_held_sends_stay_with_their_plan_across_tab_switches() {
        with_app(
            "app::approvals::tests::approvals_and_held_sends_stay_with_their_plan_across_tab_switches",
            |app, _ctx| {
                app.runtime.config.agents.push(atomics_core::config::AgentConfig {
                    name: "test".into(),
                    command: vec![],
                    kind: AgentKind::Generic,
                    busy_text: Some("esc to interrupt".into()),
                    isolation: None,
                });
                let dir = crate::test_support::tempdir().unwrap();
                let mut indices = Vec::new();
                for (goal, id) in [("One", "worker-one"), ("Two", "worker-two")] {
                    assert!(
                        request(app, json!({"cmd":"plan_set", "goal":goal, "tasks":[{"id":"a", "title":goal}]})).ok
                    );
                    let index = app.runtime.selected_plan;
                    app.runtime.plans[index].tasks[0].session = Some(id.into());
                    app.runtime.plans[index].tasks[0].status = TaskStatus::Running;
                    indices.push(index);
                    let mut worker = session(id, AgentKind::Generic, AgentStatus::Working, dir.path());
                    worker.claude_id = Some(format!("codex-{id}"));
                    worker.terminal = Some(
                        Terminal::spawn(
                            app.runtime.wake.clone(),
                            app.runtime.theme.terminal_colors(),
                            crate::test_support::program("cat"),
                            vec![],
                            dir.path(),
                            HashMap::new(),
                            100,
                        )
                        .unwrap(),
                    );
                    app.runtime.sessions.push(worker);
                }
                let (one, two) = (indices[0], indices[1]);
                let hook = |session: &str, name: &str| {
                    HookMessage {
                pane: Some(session.into()), ancestors: Vec::new(), delivered: 0, approval_reply: None,
                event: atomics_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name":name, "session_id":format!("codex-{session}"), "cwd":dir.path(),
                    "tool_name":"Bash", "tool_input":{"command":"touch permission-fixture", "description":format!("Request from {session}")}
                })).unwrap(),
            }
                };
                let permission = |session: &str| {
                    let reply = Arc::new(ipc::ApprovalReply::default());
                    let mut event = hook(session, "PermissionRequest");
                    event.approval_reply = Some(reply.clone());
                    (event, reply)
                };
                app.runtime.record_events();
                let (wait_tx, wait_rx) = channel();
                app.runtime.control(
                    ControlRequest::Wait {
                        compact: false,
                        full_sessions: false,
                        sessions: vec!["worker-one".into()],
                        statuses: vec!["needs you".into()],
                        after: Some(app.runtime.event_seq),
                        timeout_secs: Some(30),
                    },
                    wait_tx,
                );
                let (event, second_reply) = permission("worker-two");
                app.runtime.on_hook(event);
                assert!(wait_rx.try_recv().is_err(), "another plan's approval must not satisfy this waiter");
                let (event, first_reply) = permission("worker-one");
                app.runtime.on_hook(event);
                let ControlOutcome::Done(waited) =
                    wait_rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(2))).unwrap()
                else {
                    panic!("wait response")
                };
                let event = &waited.result["events"][0];
                assert_eq!(event["plan"], app.runtime.plans[one].id);
                assert_eq!(event["task"], "a");
                assert_eq!(event["session"], "worker-one");
                assert_eq!(event["approval"]["command"], "touch permission-fixture");
                assert_eq!(event["approval"]["reason"], "Request from worker-one");
                assert_eq!(event["approval"]["cwd"], json!(dir.path()));
                let other = app
                    .runtime
                    .events
                    .iter()
                    .find(|e| e.session == "worker-two" && e.approval.is_some())
                    .unwrap()
                    .to_json();
                assert_eq!(other["plan"], app.runtime.plans[two].id);
                assert_eq!(other["task"], "a");
                assert_eq!(other["approval"]["reason"], "Request from worker-two");
                app.runtime.last_status.remove("worker-one");
                app.runtime.record_events();
                let event = app.runtime.events.last().unwrap().to_json();
                assert_eq!(event["plan"], app.runtime.plans[one].id);
                assert_eq!(event["task"], "a");
                assert_eq!(event["approval"]["command"], "touch permission-fixture");
                for (index, id) in [(0, "worker-one"), (1, "worker-two")] {
                    let response = request(app, json!({"cmd":"send", "session":id, "text":format!("steer-{id}")}));
                    assert!(response.ok);
                    assert_eq!(response.result["held"], true);
                    assert_eq!(response.result["sent"], false);
                    let raw =
                        request(app, json!({"cmd":"send", "session":id, "text":format!("raw-{id}\\r"), "raw":true}));
                    assert!(!raw.ok);
                    assert!(raw.error.unwrap().contains("atomics_approve"));
                    assert_eq!(app.runtime.sessions[index].held.len(), 1);
                    assert!(app.runtime.sessions[index].pending.is_none());
                }
                assert_eq!(app.runtime.selected_plan, two);
                assert!(answer_pending(app, json!({"cmd":"approve", "session":"worker-one", "decision":"allow_always", "message":"First plan approved"})).ok);
                let output: serde_json::Value =
                    serde_json::from_str(&first_reply.wait(Duration::ZERO).stdout.unwrap()).unwrap();
                assert_eq!(output["hookSpecificOutput"]["decision"]["behavior"], "allow");
                assert_eq!(output["hookSpecificOutput"]["decision"]["message"], "First plan approved");
                assert!(app.runtime.sessions[0].approvals.is_empty());
                assert_eq!(app.runtime.sessions[1].approvals.len(), 1);
                assert_eq!(app.runtime.sessions[1].status, AgentStatus::NeedsInput);
                assert_eq!(
                    app.runtime.sessions[0].held.len(),
                    1,
                    "approval alone must not flush sends into the permission menu"
                );
                app.runtime.on_hook(hook("worker-one", "PostToolUse"));
                assert!(app.runtime.sessions[0].held.is_empty());
                assert_eq!(app.runtime.sessions[0].pending.as_ref().unwrap().text, "steer-worker-one");
                assert_eq!(app.runtime.sessions[1].held.len(), 1);
                assert!(app.runtime.sessions[1].pending.is_none());
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(2));
                while !app.runtime.sessions[0].terminal.as_ref().unwrap().screen_text(20).contains("steer-worker-one") {
                    assert!(Instant::now() < deadline, "first plan's plain message was not released");
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(!app.runtime.sessions[0].terminal.as_ref().unwrap().screen_text(20).contains("raw-worker-one"));
                assert!(!app.runtime.sessions[1].terminal.as_ref().unwrap().screen_text(20).contains("worker-one"));
                app.runtime.selected_plan = one;
                app.runtime.show_plan_fleet();
                assert!(
                answer_pending(
                    app,
                    json!({"cmd":"approve", "session":"worker-two", "decision":"deny", "message":"Second plan denied"})
                )
                .ok
            );
                let output: serde_json::Value =
                    serde_json::from_str(&second_reply.wait(Duration::ZERO).stdout.unwrap()).unwrap();
                assert_eq!(output["hookSpecificOutput"]["decision"]["behavior"], "deny");
                assert_eq!(output["hookSpecificOutput"]["decision"]["message"], "Second plan denied");
                assert_eq!(app.runtime.sessions[1].held.len(), 1);
                app.runtime.on_hook(hook("worker-two", "Interrupt"));
                assert!(app.runtime.sessions[1].held.is_empty());
                assert_eq!(app.runtime.sessions[1].pending.as_ref().unwrap().text, "steer-worker-two");
                assert_eq!(app.runtime.sessions[0].pending.as_ref().unwrap().text, "steer-worker-one");
                let (event, reply) = permission("worker-two");
                app.runtime.on_hook(event);
                assert_eq!(
                    app.runtime.sessions[1].approvals.len(),
                    1,
                    "allow_always must not leak to another plan's session"
                );
                assert!(answer_pending(app, json!({"cmd":"approve", "session":"worker-two", "decision":"allow"})).ok);
                assert!(reply.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
                let (event, reply) = permission("worker-one");
                app.runtime.on_hook(event);
                assert!(reply.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
                assert!(app.runtime.sessions[0].approvals.is_empty());
                assert_eq!(app.runtime.plans[one].tasks[0].status, TaskStatus::Running);
                assert_eq!(app.runtime.plans[two].tasks[0].status, TaskStatus::Running);
            },
        );
    }

    #[test]
    fn the_banner_shows_a_stash_warning_and_offers_no_allow_always() {
        with_app("app::approvals::tests::the_banner_shows_a_stash_warning_and_offers_no_allow_always", |app, ctx| {
            let mut worker = session("worker", AgentKind::Generic, AgentStatus::NeedsInput, &app.runtime.launch_dir);
            let warning = "stash@{1} (On atomics/t2: t2), which belongs to task t2 (session worker-two)";
            worker.approvals.push(PendingApproval {
                id: "pending".into(),
                request: ApprovalRequest {
                    tool: "Bash".into(),
                    command: Some("git stash pop 1".into()),
                    cwd: app.runtime.launch_dir.clone(),
                    reason: None,
                    input: json!({}),
                    warning: Some(warning.into()),
                },
                reply: Some(Arc::new(ipc::ApprovalReply::default())),
                native: None,
                stash: Default::default(),
            });
            app.runtime.sessions = vec![worker];
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1400.0, 900.0))),
                    ..Default::default()
                },
                |ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        let layout = layout::compute(1400.0, 900.0, 1, 0, false, 0.0, &app.runtime.config.layout);
                        app.tiles(ui, ui.max_rect(), &layout, &[0]);
                    });
                },
            );
            let text: Vec<_> = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::epaint::Shape::Text(text) => Some(text.galley.text().to_string()),
                    _ => None,
                })
                .collect();
            output.drop_without_applying_deltas();
            for label in ["Permission requested", "Allow", "Deny", warning, "git stash pop 1"] {
                assert!(text.iter().any(|text| text == label), "{label}: {text:?}");
            }
            assert!(!text.iter().any(|text| text == "Allow always"), "{text:?}");
            app.runtime.sessions.clear();
        });
    }

    /// A repository whose main checkout and two worktrees each stashed an entry, newest first:
    /// stash@{0} the main checkout's, stash@{1} task t1's (`On atomics/one: t1 before rebase`), stash@{2}
    /// task t2's (`On atomics/two: t2`). Sessions worker-one and worker-two work in those worktrees for
    /// tasks t1 and t2, and `[approvals] allow` allows any git command. Returns worker-one's worktree.
    fn stash_fleet(app: &mut Desktop<'_>, dir: &Path) -> PathBuf {
        let repo = dir.canonicalize().unwrap().join("app");
        std::fs::create_dir(&repo).unwrap();
        let git = |dir: &Path, args: &[&str]| git::git(dir, args).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "test"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        std::fs::write(repo.join("a.txt"), "base\n").unwrap();
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-q", "-m", "base"]);
        let (one, two) = (repo.with_file_name("one"), repo.with_file_name("two"));
        git::add_worktree(&repo, &two, "atomics/two", "main").unwrap();
        git::add_worktree(&repo, &one, "atomics/one", "main").unwrap();
        for (checkout, message) in [(&two, Some("t2")), (&one, Some("t1 before rebase")), (&repo, None)] {
            std::fs::write(checkout.join("a.txt"), format!("{message:?}\n")).unwrap();
            match message {
                Some(message) => git(checkout, &["stash", "push", "-q", "-m", message]),
                None => git(checkout, &["stash", "push", "-q"]),
            };
        }
        app.runtime.config.approvals.allow = vec!["git".into()];
        app.runtime.sessions = ["one", "two"]
            .into_iter()
            .map(|name| {
                let root = repo.with_file_name(name);
                let mut worker = session(&format!("worker-{name}"), AgentKind::Generic, AgentStatus::Working, &root);
                worker.worktree = Some(Worktree { root, branch: Some(format!("atomics/{name}")) });
                worker
            })
            .collect();
        let task = |id: &str, session: &str| Task { id: id.into(), session: Some(session.into()), ..Task::default() };
        app.runtime.plans =
            vec![Plan { tasks: vec![task("t1", "worker-one"), task("t2", "worker-two")], ..Plan::default() }];
        one
    }

    /// worker-one's hook asking to run `command` in its worktree `one`, and the reply it waits on.
    fn stash_permission(one: &Path, command: &str) -> (HookMessage, Arc<ipc::ApprovalReply>) {
        let reply = Arc::new(ipc::ApprovalReply::default());
        let message = HookMessage {
            pane: Some("worker-one".into()),
            ancestors: vec![],
            delivered: 0,
            approval_reply: Some(reply.clone()),
            event: atomics_core::hook::HookEvent::from_payload(&json!({
                "hook_event_name":"PermissionRequest", "session_id":"worker-one", "cwd":one,
                "tool_name":"Bash", "tool_input":{"command":command}
            }))
            .unwrap(),
        };
        (message, reply)
    }

    /// The warning a request to drop stash@{2} of [`stash_fleet`] carries for worker-one.
    const TASK_TWOS: &str = "git stash drop drops stash@{2} (On atomics/two: t2), which belongs to task t2 \
                             (session worker-two), not to this agent.";

    #[test]
    fn a_stash_drop_on_an_agents_own_menu_carries_the_warning_and_is_allowed_once() {
        with_app(
            "app::approvals::tests::a_stash_drop_on_an_agents_own_menu_carries_the_warning_and_is_allowed_once",
            |app, _ctx| {
                let dir = crate::test_support::tempdir().unwrap();
                stash_fleet(app, dir.path());
                // Codex's recorded menu, asking for a stash drop instead.
                let recorded = std::fs::read_to_string(recorded("codex-0.165-approval-screen.txt")).unwrap();
                let shown = "$ atomics queue --class test -- ./check.sh";
                assert!(recorded.contains(shown));
                let screen = dir.path().join("stash-drop.txt");
                std::fs::write(&screen, recorded.replace(shown, "$ git stash drop stash@{2}")).unwrap();
                // worker-one has no hook asking: its own menu is the request.
                app.runtime.sessions[0].terminal = Some(menu_agent(app, &[&screen]));
                app.runtime.sessions[0].status = AgentStatus::Done;
                app.runtime.busy_checked = Instant::now() - Duration::from_secs(1);
                app.runtime.track_busy();
                let pending = &app.runtime.sessions[0].approvals[0];
                assert!(pending.native.is_some());
                assert_eq!(pending.request.command.as_deref(), Some("git stash drop stash@{2}"));
                let warning = pending.request.warning.clone().unwrap();
                assert!(warning.ends_with(TASK_TWOS), "{warning}");
                let id = pending.id.clone();
                assert_eq!(app.runtime.backend().session_approval(0).unwrap().request.warning.as_ref(), Some(&warning));
                assert_eq!(app.runtime.approval_infos()[0].warning.as_ref(), Some(&warning));
                let listed = request(app, json!({"cmd":"list"})).result;
                assert_eq!(listed["sessions"][0]["approval"]["warning"], warning.as_str());
                // Polling the same menu keeps its request, and lists the stash no more.
                poll_after_quiet(app);
                assert_eq!(app.runtime.sessions[0].approvals.iter().map(|p| &p.id).collect::<Vec<_>>(), [&id]);

                // "Allow always" presses the menu's one-time Yes, and ends as a one-time allow.
                assert_eq!(app.runtime.decide(&id, Decision::AllowAlways, None).unwrap(), Disposition::Allowed);
                shows(app.runtime.sessions[0].terminal.as_ref().unwrap(), "Keys: 31 0d\n");
                assert!(app.runtime.sessions[0].allowed.is_empty());
                app.runtime.sessions.clear();
                app.runtime.plans.clear();
            },
        );
    }

    #[test]
    fn a_stash_drop_waits_names_the_entrys_owner_and_is_never_allowed_always() {
        with_app(
            "app::approvals::tests::a_stash_drop_waits_names_the_entrys_owner_and_is_never_allowed_always",
            |app, _| {
                let dir = crate::test_support::tempdir().unwrap();
                let one = stash_fleet(app, dir.path());
                let permission = |command: &str| stash_permission(&one, command);

                // Allowed by the list, a drop still waits, and says the entry is task t2's.
                let (message, reply) = permission("git stash drop stash@{2}");
                app.runtime.on_hook(message);
                assert!(!reply.expired(), "the drop must wait for a decision");
                let pending = &app.runtime.sessions[0].approvals[0];
                let warning = pending.request.warning.clone().unwrap();
                assert!(warning.starts_with(atomics_core::stash::SHARED), "{warning}");
                assert!(warning.ends_with(TASK_TWOS), "{warning}");
                // The chief's event and every client see it too.
                let event = app.runtime.events.iter().rev().find(|event| event.session == "worker-one").unwrap();
                assert_eq!(event.approval.as_ref().unwrap()["warning"], json!(warning));
                assert_eq!(app.runtime.approval_infos()[0].warning.as_ref(), Some(&warning));

                // "Allow always" answers it once, and is not remembered.
                let id = pending.id.clone();
                assert_eq!(app.runtime.decide(&id, Decision::AllowAlways, None).unwrap(), Disposition::Allowed);
                assert!(reply.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
                assert!(app.runtime.sessions[0].allowed.is_empty());
                let (message, reply) = permission("git stash drop stash@{2}");
                app.runtime.on_hook(message);
                assert!(!reply.expired() && app.runtime.sessions[0].approvals.len() == 1, "asked again");
                let id = app.runtime.sessions[0].approvals[0].id.clone();
                app.runtime.decide(&id, Decision::Deny, None).unwrap();

                // A clear names every entry's owner: the main checkout's no session accounts for.
                let (message, reply) = permission("git stash list && git stash clear");
                app.runtime.on_hook(message);
                let warning = app.runtime.sessions[0].approvals[0].request.warning.clone().unwrap();
                assert!(warning.contains("git stash clear drops all 3 entries: stash@{0} (WIP on main: "), "{warning}");
                assert!(
                warning.contains(
                    "), whose owner ATOMICS cannot tell; stash@{1} (On atomics/one: t1 before rebase), this agent's own; \
                     stash@{2} (On atomics/two: t2), which belongs to task t2 (session worker-two), not to this agent."
                ),
                "{warning}"
            );
                let id = app.runtime.sessions[0].approvals[0].id.clone();
                app.runtime.decide(&id, Decision::Deny, None).unwrap();
                assert!(reply.wait(Duration::ZERO).stdout.unwrap().contains("deny"));

                // Other commands carry no warning, and the allow list answers them.
                let (message, reply) = permission("git stash list");
                app.runtime.on_hook(message);
                assert!(reply.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
                assert!(app.runtime.sessions[0].approvals.is_empty());
                app.runtime.sessions.clear();
                app.runtime.plans.clear();
            },
        );
    }

    #[test]
    fn a_drop_whose_entry_moved_while_it_waited_is_asked_again_naming_the_new_owner() {
        with_app(
            "app::approvals::tests::a_drop_whose_entry_moved_while_it_waited_is_asked_again_naming_the_new_owner",
            |app, _| {
                let dir = crate::test_support::tempdir().unwrap();
                let one = stash_fleet(app, dir.path());
                let two = one.with_file_name("two");
                let pending = |app: &Desktop<'_>| {
                    let pending = &app.runtime.sessions[0].approvals[0];
                    (pending.id.clone(), pending.request.warning.clone().unwrap())
                };

                // worker-one asks to drop stash@{0}, the main checkout's entry, and waits.
                let (message, reply) = stash_permission(&one, "git stash drop");
                app.runtime.on_hook(message);
                let (id, warning) = pending(app);
                assert!(warning.contains("drops stash@{0} (WIP on main: "), "{warning}");
                assert!(warning.ends_with("whose owner ATOMICS cannot tell."), "{warning}");

                // Meanwhile worker-two stashes: stash@{0} is task t2's new entry now, and allowing the
                // drop would take it. Nothing is answered, and whoever decided hears whose it is now.
                std::fs::write(two.join("a.txt"), "more\n").unwrap();
                git::git(&two, &["stash", "push", "-q", "-m", "t2 again"]).unwrap();
                let events = app.runtime.events.len();
                let error = app.runtime.decide(&id, Decision::Allow, None).unwrap_err();
                assert_eq!(error.code, ErrorCode::StaleRevision, "{}", error.message);
                let now = "git stash drop drops stash@{0} (On atomics/two: t2 again), which belongs to task t2 \
                           (session worker-two), not to this agent.";
                assert!(
                    error.message.contains("the shared stash changed while this request waited"),
                    "{}",
                    error.message
                );
                assert!(error.message.contains(now), "{}", error.message);
                assert!(!reply.expired(), "the hook still waits");
                let stash = atomics_core::stash::list(&one).unwrap();
                assert_eq!(stash.len(), 4, "nothing was dropped");

                // It waits on under a new ID, with the warning as it reads now, which every waiter
                // hears; the old ID answers nothing.
                let (asked, warning) = pending(app);
                assert_ne!(asked, id);
                assert!(warning.ends_with(now), "{warning}");
                assert_eq!(app.runtime.approval_infos()[0].warning.as_ref(), Some(&warning));
                app.runtime.record_events();
                let event = app.runtime.events[events..].iter().rev().find(|event| event.session == "worker-one");
                assert_eq!(
                    event.and_then(|event| event.approval.as_ref()).and_then(|a| a["warning"].as_str()),
                    Some(warning.as_str())
                );
                assert!(app.runtime.decide(&id, Decision::Allow, None).is_err());

                // Allowed as it reads now, with the stash where it was, it goes through.
                assert_eq!(app.runtime.decide(&asked, Decision::Allow, None).unwrap(), Disposition::Allowed);
                assert!(reply.wait(Duration::ZERO).stdout.unwrap().contains("allow"));

                // A denial lets nothing run, so it needs no second look.
                let (message, reply) = stash_permission(&one, "git stash clear");
                app.runtime.on_hook(message);
                let (id, _) = pending(app);
                git::git(&two, &["stash", "drop", "-q"]).unwrap();
                assert_eq!(app.runtime.decide(&id, Decision::Deny, None).unwrap(), Disposition::Denied);
                assert!(reply.wait(Duration::ZERO).stdout.unwrap().contains("deny"));
                app.runtime.sessions.clear();
                app.runtime.plans.clear();
            },
        );
    }

    #[test]
    fn a_drop_is_warned_about_in_the_repository_its_cd_or_git_dir_names() {
        with_app(
            "app::approvals::tests::a_drop_is_warned_about_in_the_repository_its_cd_or_git_dir_names",
            |app, _| {
                let dir = crate::test_support::tempdir().unwrap();
                let one = stash_fleet(app, dir.path());
                // Another repository beside the fleet's, with a stash of its own.
                let other = one.with_file_name("other");
                std::fs::create_dir(&other).unwrap();
                let git = |args: &[&str]| git::git(&other, args).unwrap();
                git(&["init", "-q", "-b", "main"]);
                git(&["config", "user.name", "test"]);
                git(&["config", "user.email", "test@example.com"]);
                std::fs::write(other.join("b.txt"), "base\n").unwrap();
                git(&["add", "b.txt"]);
                git(&["commit", "-q", "-m", "base"]);
                std::fs::write(other.join("b.txt"), "changed\n").unwrap();
                git(&["stash", "push", "-q", "-m", "elsewhere"]);
                let theirs = "git stash drop drops stash@{0} (On main: elsewhere), whose owner ATOMICS cannot tell.";
                for command in [
                    "cd ../other && git stash drop",
                    "cd .. && cd other; git stash drop 2>&1",
                    "GIT_DIR=../other/.git git stash drop",
                    "git --git-dir ../other/.git stash drop",
                    "export GIT_DIR=../other/.git; git stash drop >/dev/null",
                ] {
                    let (message, _reply) = stash_permission(&one, command);
                    app.runtime.on_hook(message);
                    let pending = app.runtime.sessions[0].approvals.remove(0);
                    let warning = pending.request.warning.unwrap();
                    assert!(warning.ends_with(theirs), "{command}: {warning}");
                }
                // Without them it is the fleet's own stash, as before.
                let (message, _reply) = stash_permission(&one, "git stash drop 2>&1");
                app.runtime.on_hook(message);
                let warning = app.runtime.sessions[0].approvals.remove(0).request.warning.unwrap();
                assert!(warning.contains("drops stash@{0} (WIP on main: "), "{warning}");
                app.runtime.sessions.clear();
                app.runtime.plans.clear();
            },
        );
    }
}
