//! The orchestrator's plan: a goal broken into tasks that agents work on in their
//! own worktrees. The user approves the plan before anything is dispatched; agents
//! report back with `cox_report`, and the plan board shows where everything is.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub goal: String,
    pub tasks: Vec<Task>,
    /// Set by the user on the board. Nothing is dispatched before.
    pub approved: bool,
    /// Branch the tasks are merged into; `cox/plan-<goal>` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration: Option<String>,
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
    /// Repository to branch the task's worktree from.
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub status: TaskStatus,
    /// cox session working on it, once dispatched.
    pub session: Option<String>,
    pub report: Option<Report>,
    pub notes: Vec<String>,
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

impl Plan {
    pub fn load(path: &Path) -> Option<Plan> {
        std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok())
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    /// Replaces the plan, keeping progress of tasks that keep their id. A new plan
    /// needs approval again.
    pub fn replace(&mut self, goal: String, mut tasks: Vec<Task>) {
        for (i, task) in tasks.iter_mut().enumerate() {
            if task.id.trim().is_empty() {
                task.id = format!("t{}", i + 1);
            }
            if let Some(old) = self.tasks.iter().find(|t| t.id == task.id) {
                task.status = old.status;
                task.session = old.session.clone();
                task.report = old.report.clone();
                task.notes = old.notes.clone();
                if old.status != TaskStatus::Planned {
                    // A dispatched task's work is on its branch.
                    task.repo = old.repo.clone().or(task.repo.take());
                    task.branch = old.branch.clone().or(task.branch.take());
                }
            }
        }
        if goal != self.goal {
            // A new goal is a new plan, with an integration branch of its own.
            self.integration = None;
        }
        self.goal = goal;
        self.tasks = tasks;
        self.approved = false;
    }

    pub fn integration_branch(&self) -> String {
        match &self.integration {
            Some(branch) => branch.clone(),
            None => match short_slug(&self.goal) {
                slug if slug.is_empty() => "cox/plan".into(),
                slug => format!("cox/plan-{slug}"),
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

    /// Planned tasks whose dependencies are all done, in plan order.
    pub fn ready(&self) -> Vec<&Task> {
        self.tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Planned)
            .filter(|t| {
                t.depends_on.iter().all(|dep| self.tasks.iter().any(|o| &o.id == dep && o.status == TaskStatus::Done))
            })
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
    /// The first message an agent gets: the task, how to know it is done, and how
    /// to report back.
    pub fn compose_brief(&self, goal: &str) -> String {
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
        text.push_str(
            "\nWork only in this worktree and do not commit unless the task says so. When you are done, \
             blocked, or have a question, call the cox_report tool with status (done, blocked or question) \
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
    use crate::git;

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
    fn ready_respects_dependencies() {
        let mut plan = Plan::default();
        plan.replace("ship it".into(), vec![task("a", &[]), task("b", &["a"]), task("c", &[])]);
        let ready: Vec<&str> = plan.ready().iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ready, ["a", "c"]);
        plan.task_mut("a").unwrap().status = TaskStatus::Done;
        let ready: Vec<&str> = plan.ready().iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ready, ["b", "c"]);
    }

    #[test]
    fn replacing_keeps_progress_and_asks_for_approval_again() {
        let mut plan = Plan::default();
        plan.replace("v1".into(), vec![task("a", &[])]);
        plan.approved = true;
        plan.task_mut("a").unwrap().status = TaskStatus::Running;
        plan.task_mut("a").unwrap().session = Some("s1".into());
        plan.task_mut("a").unwrap().branch = Some("cox/a-do-a".into());
        plan.replace("v2".into(), vec![task("a", &[]), task("", &["zz"])]);
        assert!(!plan.approved);
        assert_eq!(plan.tasks[0].status, TaskStatus::Running);
        assert_eq!(plan.tasks[0].session.as_deref(), Some("s1"));
        assert_eq!(plan.tasks[0].branch.as_deref(), Some("cox/a-do-a"));
        assert_eq!(plan.tasks[1].id, "t2");
        assert_eq!(plan.unknown_dependencies(), [("t2".to_string(), "zz".to_string())]);
    }

    #[test]
    fn integration_branch_follows_the_goal() {
        let mut plan = Plan::default();
        assert_eq!(plan.integration_branch(), "cox/plan");
        plan.replace("IMACS v0.2: brand, CI".into(), vec![task("a", &[]), task("b", &["a"])]);
        assert_eq!(plan.integration_branch(), "cox/plan-imacs-v0-2-brand-ci");
        assert_eq!(plan.start_branch(&plan.tasks[0]), None);
        assert_eq!(plan.start_branch(&plan.tasks[1]).as_deref(), Some("cox/plan-imacs-v0-2-brand-ci"));
        plan.integration = Some("feature/v0.2".into());
        plan.replace("IMACS v0.2: brand, CI".into(), vec![task("a", &[])]);
        assert_eq!(plan.integration_branch(), "feature/v0.2");
        plan.replace("Something else".into(), vec![task("a", &[])]);
        assert_eq!(plan.integration_branch(), "cox/plan-something-else");
    }

    #[test]
    fn dependents_branch_from_the_integration_branch() {
        let (_dir, repo) = crate::git::tests::project();
        let mut plan = Plan::default();
        plan.replace("Ship the API".into(), vec![task("a", &[]), task("b", &["a"]), task("c", &[])]);
        let worktree = |id: &str| repo.with_file_name(format!("app-{id}"));
        let dispatch = |t: &Task| {
            let start = plan.start_branch(t);
            git::add_worktree_from(&repo, &worktree(&t.id), &format!("cox/{}", t.id), start.as_deref(), "main")
        };
        dispatch(&plan.tasks[0]).unwrap();
        std::fs::write(worktree("a").join("api.rs"), "pub fn api() {}\n").unwrap();
        let into = plan.integration_branch();
        git::integrate(&repo, "cox/a", "do a (a)", &into, &repo.with_file_name("app-plan"), "main").unwrap();

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
        assert!(text.contains("cox_report"));
    }

    #[test]
    fn statuses_round_trip() {
        for status in TaskStatus::ALL {
            assert_eq!(TaskStatus::parse(status.label()), Some(status));
        }
        assert_eq!(TaskStatus::parse("nope"), None);
    }
}
