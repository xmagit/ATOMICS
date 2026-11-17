//! Last-resort board previews. The authoritative host model is never changed.
use super::*;

pub(super) trait Preview: Serialize + Clone {
    fn label(&self) -> String;
    fn placeholder(&self) -> Self;

    fn fit(&self, budget: usize) -> Self {
        if json_len(self) <= budget {
            return self.clone();
        }
        eprintln!("ATOMICS: {} exceeds the board frame budget; sending a partial preview", self.label());
        self.placeholder()
    }
}

fn identity(text: &str) -> String {
    text[..text.floor_char_boundary(MAX_ID_BYTES)].into()
}

fn short(text: &str) -> String {
    let end = text.floor_char_boundary(96);
    if end < text.len() { format!("{}…", &text[..end]) } else { text.into() }
}

impl Preview for PlanInfo {
    fn label(&self) -> String {
        format!("plan {}", short(&self.id))
    }
    fn placeholder(&self) -> Self {
        Self {
            id: identity(&self.id),
            goal: format!("{} — this plan is too large to show ({})", short(&self.goal), short(&self.id)),
            status: self.status,
            approved: self.approved,
            project: self.project.as_deref().map(short),
            integration: None,
            tasks: Vec::new(),
            digest: String::new(),
            ticket: None,
            ticket_tree: None,
            notebook: Vec::new(),
            pull_request: None,
            publish: None,
        }
    }
}

impl Preview for HostInfo {
    fn label(&self) -> String {
        "host info".into()
    }
    fn placeholder(&self) -> Self {
        Self { notice: Some("Host info is too large to show. The board is still available.".into()), ..Self::default() }
    }
}

impl Preview for SessionInfo {
    fn label(&self) -> String {
        format!("session {}", short(&self.id))
    }
    fn placeholder(&self) -> Self {
        Self {
            id: identity(&self.id),
            agent: short(&self.agent),
            title: format!("{} — session details too large to show", short(&self.title)),
            cwd: short(&self.cwd),
            worktree: None,
            branch: None,
            status: self.status,
            chief: self.chief,
            task: None,
            pending_approvals: self.pending_approvals,
            status_hint: None,
            previews: Vec::new(),
        }
    }
}

impl Preview for ApprovalInfo {
    fn label(&self) -> String {
        format!("approval {}", short(&self.id))
    }
    fn placeholder(&self) -> Self {
        Self {
            // A placeholder must not authorise a command the user could not inspect.
            id: format!("oversize:{}", short(&self.id)),
            session: short(&self.session),
            tool: short(&self.tool),
            command: None,
            cwd: short(&self.cwd),
            reason: Some("This approval is too large to show. Review it on the host.".into()),
            warning: Some("This placeholder cannot be approved.".into()),
        }
    }
}

impl Preview for DiffSummary {
    fn label(&self) -> String {
        format!("diff {}", short(&self.worktree))
    }
    fn placeholder(&self) -> Self {
        Self {
            worktree: identity(&self.worktree),
            revision: self.revision,
            base_ref: format!("{} (diff too large to show)", short(&self.base_ref)),
            base: short(&self.base),
            files: Vec::new(),
            comments: Vec::new(),
            partial: true,
        }
    }
    fn fit(&self, budget: usize) -> Self {
        if json_len(self) <= budget {
            return self.clone();
        }
        eprintln!("ATOMICS: {} exceeds the board frame budget; sending a partial preview", self.label());
        let mut preview = self.clone();
        preview.partial = true;
        preview.comments.clear();
        while json_len(&preview) > budget && !preview.files.is_empty() {
            preview.files.truncate(preview.files.len() / 2);
        }
        if json_len(&preview) <= budget { preview } else { self.placeholder() }
    }
}

impl Preview for ReportInfo {
    fn label(&self) -> String {
        "task report".into()
    }
    fn placeholder(&self) -> Self {
        Self {
            status: short(&self.status),
            at: self.at,
            checks: Vec::new(),
            summary: "This task report is too large to show.".into(),
        }
    }
}

pub(super) fn event(envelope: &Envelope) -> Option<Envelope> {
    // Leave room for the envelope, cursor, epoch and request ID.
    let budget = MAX_FRAME_BYTES - 8 * 1024;
    let message = match &envelope.message {
        Message::PlanUpsert { cursor, plan } => Message::PlanUpsert { cursor: *cursor, plan: plan.fit(budget) },
        Message::HostUpsert { cursor, host } => Message::HostUpsert { cursor: *cursor, host: host.fit(budget) },
        Message::DiffSummary { cursor, summary } => {
            Message::DiffSummary { cursor: *cursor, summary: summary.fit(budget) }
        }
        Message::SessionUpsert { cursor, session } => {
            Message::SessionUpsert { cursor: *cursor, session: session.fit(budget) }
        }
        Message::ApprovalPending { cursor, approval } => {
            Message::ApprovalPending { cursor: *cursor, approval: approval.fit(budget) }
        }
        Message::TaskReport { cursor, plan, task, report } => {
            eprintln!("ATOMICS: task report {}/{} exceeds the frame budget", short(plan), short(task));
            Message::TaskReport {
                cursor: *cursor,
                plan: identity(plan),
                task: identity(task),
                report: report.fit(budget),
            }
        }
        _ => return None,
    };
    Some(Envelope { v: envelope.v, epoch: envelope.epoch.clone(), request_id: envelope.request_id.clone(), message })
}
