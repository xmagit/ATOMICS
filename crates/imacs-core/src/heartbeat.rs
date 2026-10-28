use serde::Serialize;

use crate::plan::{Plan, TaskStatus};
use crate::session::AgentStatus;

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub status: AgentStatus,
    pub status_since: u64,
    pub last_event: u64,
    pub approval: bool,
    pub held_since: Option<u64>,
    pub screen: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    AgentGone,
    Unreported,
    Approval,
    HeldMessages,
    StuckWorking,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub plan: String,
    pub task: String,
    pub session: Option<String>,
    pub kind: Kind,
}

pub fn check(plans: &[Plan], sessions: &[Session], now: u64) -> Vec<Finding> {
    let mut findings = Vec::new();
    for plan in plans.iter().filter(|p| p.is_open()) {
        for task in &plan.tasks {
            if task.status == TaskStatus::Done {
                continue;
            }
            let session = sessions.iter().find(|s| Some(&s.id) == task.session.as_ref());
            let mut add = |kind| {
                findings.push(Finding {
                    plan: plan.id.clone(),
                    task: task.id.clone(),
                    session: task.session.clone(),
                    kind,
                });
            };
            if task.status == TaskStatus::Running && session.is_none_or(|s| s.status == AgentStatus::Exited) {
                add(Kind::AgentGone);
            }
            let Some(session) = session.filter(|s| s.status != AgentStatus::Exited) else { continue };
            if matches!(session.status, AgentStatus::Idle | AgentStatus::Done)
                && now.saturating_sub(session.status_since) > 600
                && task.report.as_ref().is_none_or(|report| report.at < session.status_since)
            {
                add(Kind::Unreported);
            }
            // The footer may wrap; text quoted higher on the screen is not a menu.
            let bottom: Vec<_> = session.screen.lines().rev().take(3).collect();
            let footer: String = bottom
                .into_iter()
                .rev()
                .flat_map(str::chars)
                .filter(|c| !c.is_whitespace())
                .flat_map(char::to_lowercase)
                .collect();
            if session.approval || footer.contains("pressentertoconfirmoresctocancel") {
                add(Kind::Approval);
            }
            if session.held_since.is_some_and(|at| now.saturating_sub(at) > 60) {
                add(Kind::HeldMessages);
            }
            if session.status == AgentStatus::Working && now.saturating_sub(session.last_event) > 900 {
                add(Kind::StuckWorking);
            }
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PlanStatus, Report, Task};

    fn fixture(status: AgentStatus) -> (Vec<Plan>, Vec<Session>) {
        (
            vec![Plan {
                id: "plan-a".into(),
                tasks: vec![Task {
                    id: "task-a".into(),
                    status: TaskStatus::Running,
                    session: Some("agent".into()),
                    ..Task::default()
                }],
                ..Plan::default()
            }],
            vec![Session {
                id: "agent".into(),
                status,
                status_since: 100,
                last_event: 100,
                approval: false,
                held_since: None,
                screen: String::new(),
            }],
        )
    }

    #[test]
    fn running_task_with_missing_or_exited_agent() {
        let (mut plans, mut sessions) = fixture(AgentStatus::Exited);
        assert_eq!(check(&plans, &sessions, 100)[0].kind, Kind::AgentGone);
        sessions.clear();
        assert_eq!(check(&plans, &sessions, 100)[0].session.as_deref(), Some("agent"));
        plans[0].tasks[0].session = None;
        assert_eq!(check(&plans, &sessions, 100)[0].kind, Kind::AgentGone);
        plans[0].tasks[0].status = TaskStatus::Planned;
        assert!(check(&plans, &sessions, 100).is_empty());
    }

    #[test]
    fn idle_and_turn_done_need_a_report_after_ten_minutes() {
        for status in [AgentStatus::Idle, AgentStatus::Done] {
            let (mut plans, sessions) = fixture(status);
            assert!(check(&plans, &sessions, 700).is_empty());
            assert_eq!(check(&plans, &sessions, 701)[0].kind, Kind::Unreported);
            plans[0].tasks[0].report = Some(Report { at: 100, ..Report::default() });
            assert!(check(&plans, &sessions, 701).is_empty());
        }
    }

    #[test]
    fn progress_report_before_idle_or_turn_done_still_needs_a_new_report() {
        for status in [AgentStatus::Idle, AgentStatus::Done] {
            let (mut plans, sessions) = fixture(status);
            plans[0].tasks[0].report = Some(Report { status: "progress".into(), at: 99, ..Report::default() });
            assert!(check(&plans, &sessions, 700).is_empty());
            assert_eq!(check(&plans, &sessions, 701)[0].kind, Kind::Unreported);
            for at in [100, 101] {
                plans[0].tasks[0].report.as_mut().unwrap().at = at;
                assert!(check(&plans, &sessions, 701).is_empty());
            }
        }
    }

    #[test]
    fn approval_request_or_bottom_menu_footer() {
        let (plans, mut sessions) = fixture(AgentStatus::Working);
        sessions[0].approval = true;
        assert_eq!(check(&plans, &sessions, 100)[0].kind, Kind::Approval);
        sessions[0].approval = false;
        sessions[0].screen = "command\nPress enter to confirm or\nesc to cancel".into();
        assert_eq!(check(&plans, &sessions, 100)[0].kind, Kind::Approval);
        sessions[0].screen = "Press enter to confirm or esc to cancel\nold output\nmore output\n› prompt".into();
        assert!(check(&plans, &sessions, 100).is_empty());
        sessions[0].screen = "› 1. Normal numbered response".into();
        assert!(check(&plans, &sessions, 100).is_empty());
    }

    #[test]
    fn held_messages_older_than_one_minute() {
        let (plans, mut sessions) = fixture(AgentStatus::NeedsInput);
        sessions[0].held_since = Some(100);
        assert!(check(&plans, &sessions, 160).is_empty());
        assert_eq!(check(&plans, &sessions, 161)[0].kind, Kind::HeldMessages);
    }

    #[test]
    fn working_without_an_event_for_fifteen_minutes() {
        let (plans, mut sessions) = fixture(AgentStatus::Working);
        assert!(check(&plans, &sessions, 1000).is_empty());
        assert_eq!(check(&plans, &sessions, 1001)[0].kind, Kind::StuckWorking);
        sessions[0].last_event = 1000;
        assert!(check(&plans, &sessions, 1001).is_empty());
    }

    #[test]
    fn every_open_plan_is_checked_and_healthy_fleet_is_empty() {
        let (mut plans, sessions) = fixture(AgentStatus::Working);
        assert!(check(&plans, &sessions, 100).is_empty());
        plans.push(Plan { id: "plan-b".into(), ..plans[0].clone() });
        plans.push(Plan { id: "closed".into(), status: PlanStatus::Completed, ..plans[0].clone() });
        plans[0].tasks[0].status = TaskStatus::Done;
        let findings = check(&plans, &sessions, 1001);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].plan, "plan-b");
        assert_eq!(findings[0].task, "task-a");
        assert_eq!(serde_json::to_value(&findings[0]).unwrap()["kind"], "stuck_working");
    }
}
