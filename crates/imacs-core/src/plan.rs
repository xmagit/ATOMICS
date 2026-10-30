//! The orchestrator's plan: a goal broken into tasks that agents work on in their
//! own worktrees. The user approves the plan before anything is dispatched; agents
//! report back with `imacs_report`, and the plan board shows where everything is.

#[cfg(feature = "native")]
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub status: PlanStatus,
    pub goal: String,
    /// The project (main checkout) the plan works on; its view shows the plan.
    #[serde(default)]
    pub project: Option<std::path::PathBuf>,
    pub tasks: Vec<Task>,
    /// Set by the user on the board. Nothing is dispatched before.
    pub approved: bool,
    /// Branch the tasks are merged into; `imacs/plan-<plan-id>` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration: Option<String>,
    /// The external board (board mode) or ticket (ticket mode) this plan follows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<crate::tickets::TicketLink>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stages: Vec<crate::workflow::Stage>,
    /// Latest lifecycle progress consumed by the completion sweep, including while closed.
    #[serde(default)]
    pub finished_stage_sequence: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanStatus {
    #[default]
    Open,
    Completed,
    Archived,
}

/// Load valid plan files, including completed and archived plans, and migrate the
/// former single plan once. File names are the stable ids, including older archives.
#[cfg(feature = "native")]
pub fn load_plans(home: &Path) -> Result<(Vec<Plan>, Vec<String>)> {
    let mut errors = Vec::new();
    let dir = home.join("plans");
    std::fs::create_dir_all(&dir)?;
    let legacy = home.join("plan.json");
    if legacy.exists() && !home.join("plan.json.migrated").exists() {
        match Plan::load(&legacy) {
            Ok(mut plan) => {
                plan.integration = Some(plan.integration_branch());
                plan.id = unique_id(&plan.goal, &[], home);
                plan.status = if !plan.tasks.is_empty() && plan.count(TaskStatus::Done) == plan.tasks.len() {
                    PlanStatus::Completed
                } else {
                    PlanStatus::Open
                };
                plan.save_in(home)?;
                std::fs::rename(&legacy, home.join("plan.json.migrated"))?;
            }
            Err(err) => errors.push(format!("{}: {err:#}", legacy.display())),
        }
    }
    let mut plans = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(err) => {
                errors.push(format!("{}: {err}", dir.display()));
                continue;
            }
        };
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()).filter(|id| valid_id(id)) else {
            errors.push(format!("{}: invalid plan filename", path.display()));
            continue;
        };
        let loaded = std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|text| Ok(serde_json::from_str::<serde_json::Value>(&text)?))
            .and_then(|value| {
                let archived = value.get("status").is_none();
                let mut plan: Plan = serde_json::from_value(value)?;
                plan.id = id.to_string();
                if archived {
                    plan.status = PlanStatus::Archived;
                }
                Ok(plan)
            });
        match loaded {
            Ok(plan) => plans.push(plan),
            Err(err) => errors.push(format!("{}: {err:#}", path.display())),
        }
    }
    plans.sort_by(|a, b| a.id.cmp(&b.id));
    errors.sort();
    Ok((plans, errors))
}

#[cfg(feature = "native")]
fn valid_id(id: &str) -> bool {
    !id.is_empty() && Path::new(id).file_name().and_then(|s| s.to_str()) == Some(id)
}

#[cfg(feature = "native")]
pub fn unique_id(goal: &str, plans: &[Plan], home: &Path) -> String {
    let slug = short_slug(goal);
    let slug = if slug.is_empty() { "plan" } else { &slug };
    let mut id = slug.to_string();
    let mut suffix = 2;
    while plans.iter().any(|p| p.id == id) || home.join("plans").join(format!("{id}.json")).exists() {
        id = format!("{slug}-{suffix}");
        suffix += 1;
    }
    id
}

/// Mutating tools may omit the id only when exactly one plan is open.
pub fn resolve_plan(plans: &[Plan], id: Option<&str>) -> Result<usize, String> {
    let open: Vec<_> = plans.iter().enumerate().filter(|(_, p)| p.is_open()).collect();
    if let Some(id) = id {
        if let Some((index, _)) = open.iter().find(|(_, p)| p.id == id) {
            return Ok(*index);
        }
    } else if let [(index, _)] = open.as_slice() {
        return Ok(*index);
    }
    let names = open.iter().map(|(_, p)| format!("{} ({})", p.id, p.goal)).collect::<Vec<_>>().join(", ");
    Err(format!(
        "{} Open plans: {}",
        if id.is_some() { "Unknown open plan." } else { "Pass a plan id." },
        if names.is_empty() { "none" } else { &names }
    ))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub brief: String,
    /// Definition of done, one check per entry.
    pub acceptance: Vec<String>,
    pub depends_on: Vec<String>,
    pub agent: String,
    pub args: Vec<String>,
    /// Runs this task's agent sandboxed or bare, over its agent's and the workspace's setting.
    /// A repository that requires sandboxes still wins.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isolation: Option<crate::isolation::IsolationMode>,
    /// Repository to branch the task's worktree from.
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub status: TaskStatus,
    /// Integration branch this task was successfully merged into.
    pub integrated: Option<String>,
    /// Latest background ancestry check; rechecked after loading.
    #[serde(skip)]
    pub integration_verified: bool,
    /// IMACS session working on it, once dispatched.
    pub session: Option<String>,
    pub dispatched_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub worktree: Option<std::path::PathBuf>,
    pub resume_id: Option<String>,
    pub additions: usize,
    pub deletions: usize,
    pub report: Option<Report>,
    pub notes: Vec<String>,
    /// A persistent host-side ref guard warning, acknowledged on the board.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_alert: Option<String>,
    /// The external ticket saying what this task is for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket: Option<crate::tickets::TicketLink>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stages: Vec<crate::workflow::Stage>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    #[default]
    Planned,
    Running,
    Blocked,
    /// The agent reported done; waiting for the orchestrator or user to accept.
    Review,
    Done,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// done, blocked, question or progress.
    pub status: String,
    pub summary: String,
    pub at: u64,
}

impl Task {
    pub fn completion_line(&self) -> Option<String> {
        let elapsed = self.finished_at?.saturating_sub(self.dispatched_at?);
        Some(format!("Agent completed in {}m {}s", elapsed / 60, elapsed % 60))
    }

    pub fn reported_line(&self, now: u64) -> Option<String> {
        if self.status != TaskStatus::Review {
            return None;
        }
        self.report.as_ref().map(|report| format!("reported {} min ago", now.saturating_sub(report.at) / 60))
    }
}

impl TaskStatus {
    pub const ALL: [TaskStatus; 5] =
        [TaskStatus::Planned, TaskStatus::Running, TaskStatus::Blocked, TaskStatus::Review, TaskStatus::Done];

    pub fn label(self) -> &'static str {
        match self {
            TaskStatus::Planned => "planned",
            TaskStatus::Running => "running",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Review => "review",
            TaskStatus::Done => "done",
        }
    }

    pub fn parse(text: &str) -> Option<TaskStatus> {
        TaskStatus::ALL.into_iter().find(|s| s.label() == text.trim().to_ascii_lowercase())
    }
}

/// Id of the review task every plan ends with.
pub const REVIEW_TASK: &str = "review";

impl Plan {
    fn stage_sequence(&self) -> u64 {
        self.stages
            .iter()
            .chain(self.tasks.iter().flat_map(|task| &task.stages))
            .map(|event| event.sequence)
            .max()
            .unwrap_or(0)
    }

    pub fn reopen(&mut self) {
        self.status = PlanStatus::Open;
        self.finished_stage_sequence = self.stage_sequence();
        for task in &mut self.tasks {
            task.integration_verified = false;
        }
    }

    /// Accepted work keeps its agent until integration; final review closes on acceptance.
    pub fn finish_tasks(&mut self, now: u64, workflow: &crate::workflow::Workflow) -> bool {
        let integration = self.integration_branch();
        let stage_sequence = self.stage_sequence();
        let new_stage = stage_sequence != self.finished_stage_sequence;
        self.finished_stage_sequence = stage_sequence;
        let mut changed = false;
        for task in &mut self.tasks {
            if task.status != TaskStatus::Done && task.finished_at.take().is_some() {
                changed = true;
                if self.status == PlanStatus::Completed {
                    self.status = PlanStatus::Open;
                }
            }
            if task.status == TaskStatus::Done
                && (task.id == REVIEW_TASK
                    || task.branch.is_none()
                    || (task.integrated.as_deref() == Some(&integration) && task.integration_verified))
                && task.finished_at.is_none()
            {
                task.finished_at = Some(now);
                changed = true;
            }
        }
        if (changed || new_stage)
            && self.is_open()
            && !self.tasks.is_empty()
            && self.tasks.iter().all(|t| t.status == TaskStatus::Done && t.finished_at.is_some())
            && self.workflow_done(workflow)
        {
            self.status = PlanStatus::Completed;
            changed = true;
        }
        changed || new_stage
    }

    /// A plan milestone covers all its tasks; otherwise each implementation task must reach the gate.
    pub fn workflow_done(&self, workflow: &crate::workflow::Workflow) -> bool {
        let tasks: Vec<_> = self.tasks.iter().filter(|t| t.id != REVIEW_TASK).collect();
        workflow.reached(&self.stages) || (!tasks.is_empty() && tasks.iter().all(|t| workflow.reached(&t.stages)))
    }

    /// Plan milestones cover tasks too; compare recording order across both histories.
    pub fn latest_stage<'a>(&'a self, task: &'a Task) -> Option<&'a crate::workflow::Stage> {
        task.stages.last().into_iter().chain(self.stages.last()).max_by_key(|stage| stage.sequence)
    }

    /// External tickets are free to progress after acceptance or a recorded milestone.
    pub fn ticket_progress_expected(&self, task: Option<&str>) -> bool {
        if self.status == PlanStatus::Completed || !self.stages.is_empty() {
            return true;
        }
        match task {
            Some(id) => self
                .tasks
                .iter()
                .find(|t| t.id == id)
                .is_some_and(|t| t.status == TaskStatus::Done || !t.stages.is_empty()),
            None => {
                !self.tasks.is_empty()
                    && (self.tasks.iter().all(|t| t.status == TaskStatus::Done)
                        || self.tasks.iter().any(|t| !t.stages.is_empty()))
            }
        }
    }

    pub fn is_open(&self) -> bool {
        !self.id.is_empty() && self.status == PlanStatus::Open
    }

    #[cfg(feature = "native")]
    pub fn save_in(&self, home: &Path) -> Result<()> {
        anyhow::ensure!(valid_id(&self.id), "invalid plan id");
        self.save(&home.join("plans").join(format!("{}.json", self.id)))
    }

    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id, "goal": self.goal, "done": self.count(TaskStatus::Done),
            "total": self.tasks.len(), "approved": self.approved,
            "waiting": self.tasks.iter().filter(|t| matches!(t.status, TaskStatus::Blocked | TaskStatus::Review))
                .map(|t| &t.id).collect::<Vec<_>>(),
            "ticket": self.ticket.as_ref().map(|t| serde_json::json!({ "key": t.label(), "state": t.state })),
        })
    }

    pub fn attention(&self) -> &'static str {
        if self.count(TaskStatus::Blocked) + self.count(TaskStatus::Review) > 0 || !self.approved {
            "needs you"
        } else if !self.tasks.is_empty() && self.count(TaskStatus::Done) == self.tasks.len() {
            "all done"
        } else {
            "working"
        }
    }

    pub fn contains_session(&self, id: &str) -> bool {
        self.tasks.iter().any(|task| task.session.as_deref() == Some(id))
    }

    #[cfg(feature = "native")]
    pub fn load(path: &Path) -> Result<Plan> {
        Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
    }

    #[cfg(feature = "native")]
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    /// Updates the plan, keeping progress of tasks that keep their id. The user
    /// approves the updated plan again.
    /// The ids `replace` gives tasks: blank ids become t1, t2… by position.
    pub fn assign_task_ids(tasks: &mut [Task]) {
        for (i, task) in tasks.iter_mut().enumerate() {
            if task.id.trim().is_empty() {
                task.id = format!("t{}", i + 1);
            }
        }
    }

    pub fn replace(&mut self, goal: String, mut tasks: Vec<Task>) {
        Plan::assign_task_ids(&mut tasks);
        for task in tasks.iter_mut() {
            let old = self.tasks.iter().find(|t| t.id == task.id);
            task.ticket = crate::tickets::TicketLink::relink(task.ticket.take(), old.and_then(|o| o.ticket.as_ref()));
            if let Some(old) = old {
                task.status = old.status;
                task.stages = old.stages.clone();
                task.session = old.session.clone();
                task.report = old.report.clone();
                task.notes = old.notes.clone();
                task.integrated = old.integrated.clone();
                task.dispatched_at = old.dispatched_at;
                task.finished_at = old.finished_at;
                task.worktree = old.worktree.clone();
                task.resume_id = old.resume_id.clone();
                task.additions = old.additions;
                task.deletions = old.deletions;
                task.integration_verified = old.integration_verified;
                if old.status != TaskStatus::Planned {
                    // A dispatched task's work is on its branch.
                    task.repo = old.repo.clone().or(task.repo.take());
                    task.branch = old.branch.clone().or(task.branch.take());
                }
            }
        }
        self.goal = goal;
        self.tasks = tasks;
        self.approved = false;
        self.add_review_task();
    }

    /// Every plan ends with a fresh agent reviewing all of its work together, so
    /// nothing reaches a pull request with only its author's (or the orchestrator's) eyes on it.
    fn add_review_task(&mut self) {
        let others: Vec<String> = self.tasks.iter().filter(|t| t.id != REVIEW_TASK).map(|t| t.id.clone()).collect();
        if others.is_empty() {
            self.tasks.retain(|t| t.id != REVIEW_TASK);
            return;
        }
        let repo = self.tasks.iter().find_map(|t| t.repo.clone());
        let existing = self.tasks.iter().position(|t| t.id == REVIEW_TASK);
        let mut review = existing.map(|i| self.tasks.remove(i)).unwrap_or_else(|| Task {
            id: REVIEW_TASK.into(),
            title: "Final review of everything in this plan".into(),
            agent: "claude".into(),
            brief: "Review everything this plan changed before it is proposed as pull requests. You have not seen this \
                    work before: be the skeptical senior reviewer. Check correctness, tests, error handling, consistency \
                    with the surrounding code and the repository's conventions, and anything that would block a pull \
                    request. Do not change code. Report findings ordered by severity, each with file:line and a concrete \
                    fix, and end with a clear verdict: ready for pull requests or not."
                .into(),
            acceptance: vec![
                "every branch of the plan reviewed".into(),
                "findings ordered by severity, each with file:line and a fix".into(),
                "an explicit verdict: ready for pull requests or not".into(),
            ],
            ..Task::default()
        });
        review.depends_on = others;
        if review.repo.is_none() {
            review.repo = repo;
        }
        self.tasks.push(review);
    }

    pub fn integration_branch(&self) -> String {
        match &self.integration {
            Some(branch) => branch.clone(),
            None => match if self.id.is_empty() { short_slug(&self.goal) } else { self.id.clone() } {
                slug if slug.is_empty() => "imacs/plan".into(),
                slug => format!("imacs/plan-{slug}"),
            },
        }
    }

    /// Where a task's worktree branches from instead of base_ref: the integration
    /// branch when it builds on other tasks, so it sees their integrated work.
    pub fn start_branch(&self, task: &Task) -> Option<String> {
        (!task.depends_on.is_empty()).then(|| self.integration_branch())
    }

    pub fn task_mut(&mut self, id: &str) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    /// Dependencies whose work has not reached this plan's integration branch.
    pub fn unintegrated_dependencies<'a>(&self, task: &'a Task) -> Vec<&'a str> {
        let into = self.integration_branch();
        task.depends_on
            .iter()
            .filter(|dep| {
                !self.tasks.iter().any(|other| {
                    &other.id == *dep && other.integrated.as_deref() == Some(&into) && other.integration_verified
                })
            })
            .map(String::as_str)
            .collect()
    }

    /// Planned tasks whose dependencies are all done and integrated, in plan order.
    pub fn ready(&self) -> Vec<&Task> {
        self.tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Planned)
            .filter(|t| {
                t.depends_on.iter().all(|dep| self.tasks.iter().any(|o| &o.id == dep && o.status == TaskStatus::Done))
            })
            .filter(|t| self.unintegrated_dependencies(t).is_empty())
            .collect()
    }

    pub fn count(&self, status: TaskStatus) -> usize {
        self.tasks.iter().filter(|t| t.status == status).count()
    }

    /// Dependencies that name no task, which would keep a task from ever starting.
    pub fn unknown_dependencies(&self) -> Vec<(String, String)> {
        self.tasks
            .iter()
            .flat_map(|t| t.depends_on.iter().map(move |d| (t.id.clone(), d.clone())))
            .filter(|(_, dep)| !self.tasks.iter().any(|o| &o.id == dep))
            .collect()
    }
}

impl Task {
    pub fn set_status(&mut self, status: TaskStatus) {
        if !matches!(status, TaskStatus::Review | TaskStatus::Done) {
            self.integrated = None;
            self.integration_verified = false;
        }
        self.status = status;
    }

    /// The first message an agent gets: the task, how to know it is done, and how
    /// to report back.
    pub fn compose_brief(&self, goal: &str) -> String {
        self.compose_brief_for(goal, None)
    }

    /// `sandbox` contains the effective per-VM limits, with resource defaults filled in.
    pub fn compose_brief_for(&self, goal: &str, sandbox: Option<&crate::isolation::SandboxConfig>) -> String {
        let mut text = format!("Task {}: {}\n", self.id, self.title);
        if !goal.trim().is_empty() {
            text.push_str(&format!("Part of: {}\n", goal.trim()));
        }
        text.push('\n');
        text.push_str(self.brief.trim());
        text.push('\n');
        if !self.acceptance.is_empty() {
            text.push_str("\nDone when:\n");
            for check in &self.acceptance {
                text.push_str(&format!("- {}\n", check.trim()));
            }
        }
        if let Some(sandbox) = sandbox {
            let cpus = sandbox
                .cpus
                .filter(|cpus| *cpus != 0)
                .map_or_else(|| "all host CPUs (sbx default)".into(), |cpus| cpus.to_string());
            let memory = sandbox
                .memory
                .as_deref()
                .filter(|memory| !memory.is_empty())
                .unwrap_or("half the host memory (sbx default)");
            text.push_str(&format!(
                "\nYou run inside a Docker Sandboxes VM with {cpus} CPUs and {memory} memory. \
                 These per-sandbox limits bound builds, tests and QA. While you work, compile with the build tool's \
                 check command (for example `cargo check`) and run only the tests you touch. Run the full test suite \
                 once, at the end. Run builds, tests and QA directly inside this VM; the host's IMACS CLI and socket \
                 are not available. Use the IMACS MCP tools to report back.\n"
            ));
        } else {
            text.push_str(
            "\nThis machine is shared with other agents. While you work, compile with the build tool's check command \
             (for example `cargo check`) and run only the tests you touch. Run the full test suite once, at the end, \
             through `imacs queue --class test -- <test command>` (for .NET, \
             `imacs queue --class test -- dotnet test` for whole suites), and start QA app instances through \
             `imacs queue --class qa -- <command>`; both wait for a free slot, so give them a long timeout \
             (or run them in the background) instead of the default two minutes. In a sandbox that blocks the \
             IMACS socket, request to run them outside the sandbox.\n",
        );
        }
        text.push_str(
            "\nWork only in this worktree and do not commit unless the task says so. When you are done, \
             blocked, or have a question, call the imacs_report tool with status (done, blocked or question) \
             and a short summary: what changed and how you checked it.",
        );
        text
    }
}

/// A short branch-friendly version of a task title or goal.
pub fn short_slug(title: &str) -> String {
    let lower: String = title.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let mut slug = lower.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    slug.truncate(40);
    slug.trim_end_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "native")]
    use crate::git;

    #[test]
    fn reopened_plan_consumes_old_gate_progress_until_a_new_stage_or_task_change() {
        use crate::workflow::{Stage, Workflow};
        let workflow = Workflow { done: "ci_passed".into(), ..Default::default() };
        let mut plan = Plan {
            id: "reopen".into(),
            tasks: vec![Task { id: "a".into(), status: TaskStatus::Done, ..Default::default() }],
            ..Default::default()
        };
        assert!(plan.finish_tasks(1, &workflow));
        assert_eq!(plan.status, PlanStatus::Open);
        plan.tasks[0].stages.push(Stage { stage: "ci_passed".into(), sequence: 1, ..Default::default() });
        assert!(plan.finish_tasks(2, &workflow), "new task stage completes already finished tasks");
        assert_eq!(plan.status, PlanStatus::Completed);
        plan.reopen();
        // The consumed progress persists across saving/loading an explicitly reopened plan.
        let mut plan: Plan = serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap();
        assert!(!plan.finish_tasks(3, &workflow));
        assert!(!plan.finish_tasks(4, &workflow));
        assert_eq!(plan.status, PlanStatus::Open);

        plan.stages.push(Stage { stage: "merged".into(), sequence: 2, ..Default::default() });
        assert!(plan.finish_tasks(5, &workflow), "a newly recorded plan stage is fresh progress");
        assert_eq!(plan.status, PlanStatus::Completed);
        // Events recorded while completed must also be consumed before a later reopen.
        plan.stages.push(Stage { stage: "deployed".into(), sequence: 3, ..Default::default() });
        assert!(plan.finish_tasks(6, &workflow));
        plan.reopen();
        assert!(!plan.finish_tasks(7, &workflow));

        plan.tasks[0].set_status(TaskStatus::Running);
        assert!(plan.finish_tasks(8, &workflow));
        assert_eq!(plan.status, PlanStatus::Open);
        plan.tasks[0].set_status(TaskStatus::Done);
        assert!(plan.finish_tasks(9, &workflow), "new task completion can satisfy the existing gate");
        assert_eq!(plan.status, PlanStatus::Completed);

        // Older saved plans have no consumed-stage marker yet; reopening snapshots their history.
        let mut legacy = serde_json::to_value(&plan).unwrap();
        legacy.as_object_mut().unwrap().remove("finished_stage_sequence");
        let mut legacy: Plan = serde_json::from_value(legacy).unwrap();
        legacy.reopen();
        assert!(!legacy.finish_tasks(10, &workflow));
        assert_eq!(legacy.status, PlanStatus::Open);
    }

    #[test]
    fn later_completion_gate_preserves_acceptance_and_integration_requirements() {
        use crate::workflow::{Stage, Workflow};
        let workflow = Workflow { done: "ci_passed".into(), ..Default::default() };
        let mut plan = Plan {
            id: "gate".into(),
            tasks: vec![Task {
                id: "a".into(),
                status: TaskStatus::Done,
                branch: Some("work".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        plan.stages.push(Stage { stage: "ci_passed".into(), ..Default::default() });
        assert!(!plan.finish_tasks(1, &workflow), "CI cannot bypass integration");
        plan.tasks[0].integrated = Some(plan.integration_branch());
        plan.tasks[0].integration_verified = true;
        assert!(plan.finish_tasks(2, &workflow));
        assert_eq!(plan.status, PlanStatus::Completed);
        assert_eq!(plan.tasks[0].status, TaskStatus::Done);
    }

    #[test]
    fn acceptance_waits_for_integration_and_final_review_before_completing_plan() {
        let mut plan = Plan {
            id: "ship".into(),
            tasks: vec![
                Task {
                    id: "code".into(),
                    branch: Some("code".into()),
                    status: TaskStatus::Done,
                    dispatched_at: Some(100),
                    ..Task::default()
                },
                Task {
                    id: REVIEW_TASK.into(),
                    branch: Some("review".into()),
                    status: TaskStatus::Review,
                    ..Task::default()
                },
            ],
            ..Plan::default()
        };
        assert!(!plan.finish_tasks(890, &crate::workflow::Workflow::default()));
        assert!(plan.tasks[0].finished_at.is_none());
        plan.tasks[0].integrated = Some("wrong-plan".into());
        assert!(!plan.finish_tasks(890, &crate::workflow::Workflow::default()));
        plan.tasks[0].integrated = Some(plan.integration_branch());
        assert!(
            !plan.finish_tasks(890, &crate::workflow::Workflow::default()),
            "an unverified integration marker must not close an agent"
        );
        plan.tasks[0].integration_verified = true;
        assert!(plan.finish_tasks(890, &crate::workflow::Workflow::default()));
        assert_eq!(plan.tasks[0].completion_line().as_deref(), Some("Agent completed in 13m 10s"));
        assert_eq!(plan.status, PlanStatus::Open);
        plan.tasks[1].status = TaskStatus::Done;
        assert!(plan.finish_tasks(1000, &crate::workflow::Workflow::default()));
        assert_eq!(plan.status, PlanStatus::Completed);
        assert_eq!(plan.tasks[0].finished_at, Some(890));
        assert!(!plan.finish_tasks(2000, &crate::workflow::Workflow::default()));
        plan.tasks[0].integration_verified = false;
        #[cfg(feature = "native")]
        {
            let dir = crate::test_support::tempdir().unwrap();
            plan.save_in(dir.path()).unwrap();
            assert_eq!(load_plans(dir.path()).unwrap().0, [plan.clone()]);
        }
        plan.status = PlanStatus::Open;
        assert!(!plan.finish_tasks(2500, &crate::workflow::Workflow::default()));
        assert_eq!(plan.status, PlanStatus::Open, "an explicitly reopened plan stays open");
        plan.tasks[0].status = TaskStatus::Running;
        assert!(plan.finish_tasks(3000, &crate::workflow::Workflow::default()));
        assert_eq!(plan.status, PlanStatus::Open);
        assert!(plan.tasks[0].finished_at.is_none());
    }

    #[test]
    fn durations_reports_and_completion_metadata_survive_plan_edits() {
        let mut task = Task {
            id: "code".into(),
            dispatched_at: Some(100),
            finished_at: Some(890),
            worktree: Some("/work".into()),
            resume_id: Some("resume".into()),
            additions: 412,
            deletions: 88,
            report: Some(Report { at: 100, ..Report::default() }),
            ..Task::default()
        };
        assert_eq!(task.reported_line(1600), None);
        task.status = TaskStatus::Review;
        assert_eq!(task.reported_line(1600).as_deref(), Some("reported 25 min ago"));
        assert_eq!(task.reported_line(0).as_deref(), Some("reported 0 min ago"));
        let mut plan = Plan { tasks: vec![task.clone()], ..Plan::default() };
        plan.replace("new goal".into(), vec![Task { id: "code".into(), ..Task::default() }]);
        assert_eq!(plan.tasks[0], task);
        let old: Task = serde_json::from_str(r#"{"id":"old"}"#).unwrap();
        assert!(old.dispatched_at.is_none() && old.finished_at.is_none());
        assert_eq!(old.completion_line(), None);
        let mut empty = Plan { id: "empty".into(), ..Plan::default() };
        assert!(!empty.finish_tasks(1, &crate::workflow::Workflow::default()));
        assert_eq!(empty.status, PlanStatus::Open);
    }

    fn task(id: &str, deps: &[&str]) -> Task {
        Task {
            id: id.into(),
            title: format!("do {id}"),
            depends_on: deps.iter().map(|d| d.to_string()).collect(),
            agent: "claude".into(),
            ..Task::default()
        }
    }

    #[test]
    #[cfg(feature = "native")]
    fn migrates_once_without_overwriting_existing_or_archived_plans() {
        let dir = crate::test_support::tempdir().unwrap();
        let mut old = Plan::default();
        old.replace("Ship it".into(), vec![task("a", &[])]);
        old.approved = true;
        old.tasks[0].session = Some("legacy-agent".into());
        old.save(&dir.path().join("plan.json")).unwrap();
        let archive = Plan { id: "ship-it".into(), status: PlanStatus::Archived, ..old.clone() };
        archive.save_in(dir.path()).unwrap();
        let plans = load_plans(dir.path()).unwrap().0;
        assert_eq!(plans.len(), 2);
        let migrated = plans.iter().find(|p| p.is_open()).unwrap();
        assert_eq!(migrated.id, "ship-it-2");
        assert!(migrated.approved);
        assert_eq!(migrated.integration_branch(), old.integration_branch());
        assert_eq!(migrated.tasks[0].session.as_deref(), Some("legacy-agent"));
        assert_eq!(plans.iter().find(|p| p.id == "ship-it").unwrap(), &archive);
        assert!(!dir.path().join("plan.json").exists());
        assert!(dir.path().join("plan.json.migrated").exists());
        assert_eq!(load_plans(dir.path()).unwrap().0, plans);
    }

    #[test]
    #[cfg(feature = "native")]
    fn completed_legacy_plans_migrate_closed_while_unfinished_and_empty_plans_stay_open() {
        for statuses in [vec![TaskStatus::Done, TaskStatus::Done], vec![TaskStatus::Done, TaskStatus::Review], vec![]] {
            let dir = crate::test_support::tempdir().unwrap();
            let plan = Plan {
                goal: "Legacy".into(),
                tasks: statuses
                    .iter()
                    .enumerate()
                    .map(|(i, status)| Task { id: format!("t{i}"), status: *status, ..Task::default() })
                    .collect(),
                ..Plan::default()
            };
            plan.save(&dir.path().join("plan.json")).unwrap();
            let (loaded, errors) = load_plans(dir.path()).unwrap();
            assert!(errors.is_empty());
            let completed = !statuses.is_empty() && statuses.iter().all(|s| *s == TaskStatus::Done);
            assert_eq!(loaded[0].status, if completed { PlanStatus::Completed } else { PlanStatus::Open });
            assert_eq!(loaded[0].is_open(), !completed);
        }
    }

    #[test]
    #[cfg(feature = "native")]
    fn saving_replaces_the_file_atomically_and_keeps_existing_data_on_failure() {
        use std::io::Read;
        let dir = crate::test_support::tempdir().unwrap();
        let path = dir.path().join("plan.json");
        let mut plan = Plan { goal: "Original".into(), ..Plan::default() };
        plan.save(&path).unwrap();
        let original = std::fs::read_to_string(&path).unwrap();
        let mut reader = std::fs::File::open(&path).unwrap();
        plan.goal = "Updated".into();
        plan.save(&path).unwrap();
        let mut snapshot = String::new();
        reader.read_to_string(&mut snapshot).unwrap();
        assert_eq!(snapshot, original, "existing readers keep a complete prior file");
        assert_eq!(Plan::load(&path).unwrap(), plan);
        let tmp = path.with_extension("json.tmp");
        assert!(!tmp.exists());
        std::fs::create_dir(&tmp).unwrap();
        let saved = std::fs::read(&path).unwrap();
        plan.goal = "Unwritable".into();
        assert!(plan.save(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), saved);
    }

    #[test]
    fn leaving_review_or_done_clears_integration() {
        for status in TaskStatus::ALL {
            let mut task = Task {
                status: TaskStatus::Done,
                integrated: Some("integration".into()),
                integration_verified: true,
                ..Task::default()
            };
            task.set_status(status);
            assert_eq!(task.integrated.is_some(), matches!(status, TaskStatus::Review | TaskStatus::Done));
            assert_eq!(task.integration_verified, matches!(status, TaskStatus::Review | TaskStatus::Done));
        }
    }

    #[test]
    #[cfg(feature = "native")]
    fn older_archives_without_status_load_but_do_not_get_open_tabs() {
        let dir = crate::test_support::tempdir().unwrap();
        let folder = dir.path().join("plans");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("imacs-v0.3.json"), r#"{"goal":"Old plan", "tasks":[], "approved":true}"#).unwrap();
        let plans = load_plans(dir.path()).unwrap().0;
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].id, "imacs-v0.3");
        assert_eq!(plans[0].status, PlanStatus::Archived);
        assert!(!plans[0].is_open());
        plans[0].save_in(dir.path()).unwrap();
        assert_eq!(load_plans(dir.path()).unwrap().0, plans);
    }

    #[test]
    #[cfg(feature = "native")]
    fn storage_keeps_plans_independent_and_reports_unreadable_files() {
        let dir = crate::test_support::tempdir().unwrap();
        let mut plans = Vec::new();
        for goal in ["API", "API", "UI"] {
            let mut plan = Plan { id: unique_id(goal, &plans, dir.path()), ..Plan::default() };
            plan.replace(goal.into(), vec![task("a", &[])]);
            plan.save_in(dir.path()).unwrap();
            plans.push(plan);
        }
        assert_eq!(plans.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["api", "api-2", "ui"]);
        assert_ne!(plans[0].integration_branch(), plans[1].integration_branch());
        plans[0].approved = true;
        plans[0].tasks[0].status = TaskStatus::Done;
        plans[0].save_in(dir.path()).unwrap();
        plans[1].status = PlanStatus::Completed;
        plans[1].save_in(dir.path()).unwrap();
        std::fs::write(dir.path().join("plans/broken.json"), "{no").unwrap();
        std::fs::write(dir.path().join("plans/note.txt"), "irrelevant").unwrap();
        std::fs::create_dir(dir.path().join("plans/unreadable.json")).unwrap();
        let (loaded, errors) = load_plans(dir.path()).unwrap();
        assert_eq!(errors.len(), 2);
        assert!(errors.iter().any(|e| e.contains("plans/broken.json")));
        assert!(errors.iter().any(|e| e.contains("plans/unreadable.json")));
        assert_eq!(loaded, plans);
        assert_eq!(loaded.iter().filter(|p| p.is_open()).count(), 2);
        assert!(loaded[0].approved);
        assert!(!loaded[2].approved);
        assert_eq!(loaded[2].tasks[0].status, TaskStatus::Planned);
        let unsafe_plan = Plan { id: "../escape".into(), ..Plan::default() };
        assert!(unsafe_plan.save_in(dir.path()).is_err());
    }

    #[test]
    fn selection_requires_id_when_ambiguous_and_names_open_plans() {
        let mut plans = vec![Plan { id: "one".into(), goal: "First goal".into(), ..Plan::default() }];
        assert_eq!(resolve_plan(&plans, None), Ok(0));
        plans.push(Plan { id: "two".into(), goal: "Second goal".into(), ..Plan::default() });
        plans.push(Plan { id: "old".into(), status: PlanStatus::Archived, ..Plan::default() });
        for id in [None, Some("missing"), Some("old")] {
            let error = resolve_plan(&plans, id).unwrap_err();
            assert!(error.contains("one (First goal)"));
            assert!(error.contains("two (Second goal)"));
            assert!(!error.contains("old ("));
        }
        assert_eq!(resolve_plan(&plans, Some("two")), Ok(1));
    }

    #[test]
    fn summary_and_tab_attention_follow_only_this_plans_tasks() {
        let mut plan = Plan { id: "one".into(), ..Plan::default() };
        plan.replace("First".into(), vec![task("a", &[])]);
        assert_eq!(plan.attention(), "needs you");
        plan.approved = true;
        assert_eq!(plan.attention(), "working");
        plan.tasks[0].status = TaskStatus::Review;
        assert_eq!(plan.attention(), "needs you");
        assert_eq!(plan.summary()["waiting"], serde_json::json!(["a"]));
        for task in &mut plan.tasks {
            task.status = TaskStatus::Done;
        }
        assert_eq!(plan.attention(), "all done");
        assert_eq!(plan.summary()["done"], 2);
        assert_eq!(plan.summary()["total"], 2);
    }

    #[test]
    fn rewording_goal_preserves_task_and_review_progress() {
        let mut plan = Plan::default();
        plan.replace("Old goal".into(), vec![task("a", &[])]);
        plan.approved = true;
        plan.integration = Some("existing-integration".into());
        for task in &mut plan.tasks {
            task.status = TaskStatus::Blocked;
            task.session = Some("existing-agent".into());
            task.branch = Some("existing-branch".into());
            task.notes.push("existing note".into());
            task.report = Some(Report { status: "blocked".into(), summary: "waiting".into(), at: 42 });
            task.integrated = Some("existing-integration".into());
        }
        plan.replace("Reworded goal".into(), vec![task("a", &[]), task(REVIEW_TASK, &[])]);
        assert_eq!(plan.goal, "Reworded goal");
        assert!(!plan.approved);
        assert_eq!(plan.integration.as_deref(), Some("existing-integration"));
        for task in &plan.tasks {
            assert_eq!(task.status, TaskStatus::Blocked);
            assert_eq!(task.session.as_deref(), Some("existing-agent"));
            assert_eq!(task.branch.as_deref(), Some("existing-branch"));
            assert_eq!(task.notes, ["existing note"]);
            assert_eq!(task.report, Some(Report { status: "blocked".into(), summary: "waiting".into(), at: 42 }));
            assert_eq!(task.integrated.as_deref(), Some("existing-integration"));
        }
    }

    #[test]
    fn fleet_includes_only_linked_sessions_in_any_task_state() {
        let mut plan = Plan::default();
        for status in TaskStatus::ALL {
            let linked = format!("linked-{}", status.label());
            plan.tasks = vec![Task { session: Some(linked.clone()), status, ..Task::default() }, Task::default()];
            assert!(plan.contains_session(&linked), "{status:?}");
            assert!(!plan.contains_session("unrelated"));
            assert!(!plan.contains_session(""), "an undispatched task must not match an empty session id");
        }
    }

    #[test]
    fn ready_respects_dependencies() {
        let mut plan = Plan::default();
        plan.replace("ship it".into(), vec![task("a", &[]), task("b", &["a"]), task("c", &[])]);
        let ready: Vec<&str> = plan.ready().iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ready, ["a", "c"]);
        assert_eq!(plan.tasks.last().unwrap().id, REVIEW_TASK);
        plan.task_mut("a").unwrap().status = TaskStatus::Done;
        let ready: Vec<&str> = plan.ready().iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ready, ["c"], "done alone does not make the work available");
        assert_eq!(plan.unintegrated_dependencies(&plan.tasks[1]), ["a"]);
        plan.task_mut("a").unwrap().integrated = Some(plan.integration_branch());
        assert!(plan.ready().iter().all(|t| t.id != "b"), "an unchecked marker is not ready");
        plan.task_mut("a").unwrap().integration_verified = true;
        let ready: Vec<&str> = plan.ready().iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ready, ["b", "c"]);
        let loaded: Plan = serde_json::from_value(serde_json::to_value(&plan).unwrap()).unwrap();
        assert!(!loaded.tasks[0].integration_verified, "freshness is never persisted across restarts");
        assert_eq!(loaded.ready().iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["c"]);
        plan.integration = Some("another-integration".into());
        assert_eq!(plan.ready().iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["c"]);
    }

    #[test]
    fn replacing_keeps_progress_and_asks_for_approval_again() {
        let mut plan = Plan::default();
        plan.replace("v1".into(), vec![task("a", &[])]);
        plan.approved = true;
        plan.task_mut("a").unwrap().status = TaskStatus::Running;
        plan.task_mut("a").unwrap().session = Some("s1".into());
        plan.task_mut("a").unwrap().branch = Some("imacs/a-do-a".into());
        plan.replace("v1".into(), vec![task("a", &[]), task("", &["zz"])]);
        assert!(!plan.approved);
        assert_eq!(plan.tasks[0].status, TaskStatus::Running);
        assert_eq!(plan.tasks[0].session.as_deref(), Some("s1"));
        assert_eq!(plan.tasks[0].branch.as_deref(), Some("imacs/a-do-a"));
        assert_eq!(plan.tasks[1].id, "t2");
        assert_eq!(plan.unknown_dependencies(), [("t2".to_string(), "zz".to_string())]);
        assert_eq!(plan.tasks.len(), 3, "plus the review task");
    }

    #[test]
    fn a_tasks_isolation_is_saved_and_follows_the_chiefs_updates() {
        use crate::isolation::IsolationMode::Bare;
        let mut plan = Plan::default();
        let bare = Task { isolation: Some(Bare), ..task("a", &[]) };
        plan.replace("g".into(), vec![bare.clone()]);
        plan.task_mut("a").unwrap().status = TaskStatus::Running;
        let saved = serde_json::to_value(&plan).unwrap();
        assert_eq!(saved["tasks"][0]["isolation"], "bare");
        assert!(saved["tasks"][1].get("isolation").is_none(), "unset isolation is not written");
        let loaded: Plan = serde_json::from_value(saved).unwrap();
        assert_eq!(loaded.tasks[0].isolation, Some(Bare));
        let old: Task = serde_json::from_str(r#"{"id":"a","agent":"codex"}"#).unwrap();
        assert_eq!(old.isolation, None, "tasks saved before the field load");
        // imacs_plan_set replaces the setting, keeping the task's progress.
        plan.replace("g".into(), vec![task("a", &[])]);
        assert_eq!((plan.tasks[0].isolation, plan.tasks[0].status), (None, TaskStatus::Running));
        plan.replace("g".into(), vec![bare]);
        assert_eq!(plan.tasks[0].isolation, Some(Bare));
    }

    #[test]
    fn every_plan_ends_with_a_review_of_all_tasks() {
        let mut plan = Plan::default();
        plan.replace("g".into(), vec![task("a", &[]), task("b", &["a"])]);
        let review = plan.tasks.last().unwrap();
        assert_eq!(review.id, REVIEW_TASK);
        assert_eq!(review.depends_on, ["a", "b"]);
        plan.replace("g".into(), vec![task("a", &[])]);
        assert_eq!(plan.tasks.iter().filter(|t| t.id == REVIEW_TASK).count(), 1);
        assert_eq!(plan.tasks.last().unwrap().depends_on, ["a"]);
        plan.replace("g".into(), Vec::new());
        assert!(plan.tasks.is_empty());
    }

    #[test]
    fn integration_branch_defaults_to_goal_and_preserves_explicit_override() {
        let mut plan = Plan::default();
        assert_eq!(plan.integration_branch(), "imacs/plan");
        plan.replace("IMACS v0.2: brand, CI".into(), vec![task("a", &[]), task("b", &["a"])]);
        assert_eq!(plan.integration_branch(), "imacs/plan-imacs-v0-2-brand-ci");
        assert_eq!(plan.start_branch(&plan.tasks[0]), None);
        assert_eq!(plan.start_branch(&plan.tasks[1]).as_deref(), Some("imacs/plan-imacs-v0-2-brand-ci"));
        plan.integration = Some("feature/v0.2".into());
        plan.replace("IMACS v0.2: brand, CI".into(), vec![task("a", &[])]);
        assert_eq!(plan.integration_branch(), "feature/v0.2");
        plan.replace("Something else".into(), vec![task("a", &[])]);
        assert_eq!(plan.integration_branch(), "feature/v0.2");
    }

    #[test]
    #[cfg(feature = "native")]
    fn dependents_branch_from_the_integration_branch() {
        let (_dir, repo) = crate::git::tests::project();
        let mut plan = Plan::default();
        plan.replace("Ship the API".into(), vec![task("a", &[]), task("b", &["a"]), task("c", &[])]);
        let worktree = |id: &str| repo.with_file_name(format!("app-{id}"));
        let dispatch = |t: &Task| {
            let start = plan.start_branch(t);
            git::add_worktree_from(&repo, &worktree(&t.id), &format!("imacs/{}", t.id), start.as_deref(), "main")
        };
        dispatch(&plan.tasks[0]).unwrap();
        std::fs::write(worktree("a").join("api.rs"), "pub fn api() {}\n").unwrap();
        let into = plan.integration_branch();
        git::integrate(&repo, "imacs/a", "do a (a)", &into, &repo.with_file_name("app-plan"), "main").unwrap();

        dispatch(&plan.tasks[1]).unwrap();
        dispatch(&plan.tasks[2]).unwrap();
        assert!(worktree("b").join("api.rs").exists(), "b builds on a's integrated work");
        assert!(!worktree("c").join("api.rs").exists(), "c has no dependencies and starts from main");
    }

    #[test]
    fn brief_tells_the_agent_how_to_report() {
        let mut t = task("a", &[]);
        t.brief = "Add a --json flag.".into();
        t.acceptance = vec!["tests pass".into()];
        let text = t.compose_brief("Better CLI");
        assert!(
            text.starts_with("Task a: do a\nPart of: Better CLI\n\nAdd a --json flag.\n\nDone when:\n- tests pass\n")
        );
        assert!(text.contains("imacs_report"));
        assert!(text.contains("imacs queue --class test -- dotnet test"));
    }

    #[test]
    fn statuses_round_trip() {
        for status in TaskStatus::ALL {
            assert_eq!(TaskStatus::parse(status.label()), Some(status));
        }
        assert_eq!(TaskStatus::parse("nope"), None);
    }
}
