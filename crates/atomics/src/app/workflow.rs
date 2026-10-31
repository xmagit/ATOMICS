use super::*;
use atomics_core::tickets::{Author, TicketLink};
use atomics_core::workflow::Stage;

impl State {
    pub(super) fn record_stage(
        &mut self,
        index: usize,
        task: Option<&str>,
        event: Stage,
        author: Author,
    ) -> Result<serde_json::Value, String> {
        let workflow = self.plan_workflow(&self.plans[index]);
        workflow.validate()?;
        if !workflow.allows(&event.stage) {
            return Err(self.cleaner().error(&format!("unknown workflow stage {:?}", event.stage)));
        }
        if self.plans[index].status == PlanStatus::Archived {
            return Err("Reopen the plan before reporting a stage.".into());
        }
        let cleaner = self.cleaner();
        let sequence = self.plans[index]
            .stages
            .iter()
            .chain(self.plans[index].tasks.iter().flat_map(|t| &t.stages))
            .map(|event| event.sequence)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let event = Stage {
            sequence,
            stage: event.stage,
            url: event.url.map(|url| cleaner.url(&url)),
            note: event.note.map(|note| cleaner.field(&note)),
        };
        let plan = &mut self.plans[index];
        let history = if let Some(id) = task {
            let task = plan.task_mut(id).ok_or_else(|| format!("no task {id}"))?;
            if task.status != TaskStatus::Done && !(event.stage == "integrated" && task.status == TaskStatus::Review) {
                return Err("Accept the task before reporting a lifecycle stage.".into());
            }
            &mut task.stages
        } else {
            if plan.tasks.is_empty() || plan.tasks.iter().any(|t| t.status != TaskStatus::Done) {
                return Err("Accept every task before reporting a plan lifecycle stage.".into());
            }
            &mut plan.stages
        };
        let recorded = !history
            .last()
            .is_some_and(|old| old.stage == event.stage && old.url == event.url && old.note == event.note);
        if recorded {
            history.push(event.clone());
            self.save_plan(index);
            let body = atomics_core::tickets::stage_comment(&author, &event);
            self.lifecycle_comment(index, task, &author, body);
            self.finish_plan_agents();
            self.push_ticket_states(None);
        }
        Ok(json!({"plan": self.plans[index].id, "task": task, "stage": event, "recorded": recorded,
            "status": self.plans[index].status}))
    }

    /// An event must identify exactly one saved target; never guess from the selected tab.
    pub(super) fn stage_target(
        &self,
        plan: Option<&str>,
        task: Option<&str>,
        reference: Option<&TicketLink>,
    ) -> Result<(usize, Option<String>), String> {
        if plan.is_none() && reference.is_none() {
            return Err("Give a plan id or a linked ticket id/key.".into());
        }
        let config = self.ticket_config();
        let matches = |link: Option<&TicketLink>| {
            reference.is_none_or(|r| {
                link.is_some_and(|link| {
                    (!r.system.is_empty() || config.as_ref().is_none_or(|config| config.accepts(link)))
                        && link.matches(Some(&r.system), Some(&r.id), r.key.as_deref())
                })
            })
        };
        let mut targets = Vec::new();
        for (index, p) in self.plans.iter().enumerate().filter(|(_, p)| p.status != PlanStatus::Archived) {
            if plan.is_some_and(|id| id != p.id) {
                continue;
            }
            if task.is_none() && matches(p.ticket.as_ref()) {
                targets.push((index, None));
            }
            if task.is_some() || reference.is_some() {
                for t in p.tasks.iter().filter(|t| task.is_none_or(|id| t.id == id)) {
                    if matches(t.ticket.as_ref()) {
                        targets.push((index, Some(t.id.clone())));
                    }
                }
            }
        }
        match targets.as_slice() {
            [target] => Ok(target.clone()),
            [] => Err("No matching plan or task for stage event.".into()),
            _ => Err("Ambiguous stage event; give a plan and task explicitly.".into()),
        }
    }
}
