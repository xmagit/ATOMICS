use super::*;
use atomics_core::control::clip_list;

impl State {
    /// Save a validated single-task edit without rebuilding or redispatching the plan.
    pub(super) fn save_task_edit(&mut self, index: usize, mut candidate: Plan, id: &str) -> ControlResponse {
        if candidate.project.is_none() {
            candidate.project = self.plan_project(&candidate);
        }
        let (problems, mut warnings) = self.plan_problems(Some(index), &candidate);
        if !problems.is_empty() {
            return ControlResponse::error(format!(
                "Task not saved:\n- {}",
                clip_list(&problems, 600, 20).join("\n- ")
            ));
        }
        self.usage.drain();
        warnings.extend(self.quota_warnings(&candidate));
        let task = candidate.task_mut(id).expect("edited task");
        let old_link = self.plans[index].tasks.iter().find(|task| task.id == id).and_then(|task| task.ticket.as_ref());
        if task.ticket.as_ref().is_some_and(|link| old_link.is_none_or(|old| !old.same_ticket(link))) {
            self.relinked((candidate.id.clone(), Some(id.to_string())));
        }
        let task = candidate.task_mut(id).expect("edited task");
        task.ticket = self.stamp_system(task.ticket.take());
        self.plans[index] = candidate;
        self.save_plan(index);
        self.plan_event(index, "edited");
        self.remote_request_approval(index, false);
        self.changed_plans.insert(self.plans[index].id.clone());
        let plan = self.plans[index].id.clone();
        self.refresh_tickets(Some(&plan), None);
        let mut result = self.plan_mutation_json(index, Some(id));
        let task = self.plans[index].tasks.iter().find(|task| task.id == id).expect("edited task");
        result["task"]["approved"] = json!(self.plans[index].task_approved(task));
        result["task"]["start_on_approval"] = json!(task.start_on_approval);
        if !warnings.is_empty() {
            result["warnings"] = json!(clip_list(&warnings, 600, 20));
        }
        ControlResponse::ok(result)
    }

    pub(super) fn record_task_message(&mut self, session: usize, text: &str) {
        let id = &self.sessions[session].id;
        let owner = self.plans.iter().enumerate().find_map(|(index, plan)| {
            plan.tasks.iter().position(|task| task.session.as_ref() == Some(id)).map(|task| (index, task))
        });
        if let Some((index, task)) = owner {
            self.plans[index].tasks[task].notes.push(format!("Message to agent:\n{text}"));
            self.save_plan(index);
        }
    }
}
