//! What `plan_get` answers about one plan: a compact view of every task that stays inside what
//! an MCP client accepts, and one task in full on request.
use atomics_core::control::{MAX_PLAN_VIEW_CHARS, clip, clip_json, clip_list, shown_len};
use atomics_core::plan::{MAX_REPORT_CHECKS, REPORT_DATA_NOTICE, report_text};

use super::*;

/// Reports stay inside JSON string values and carry an explicit trust boundary.
fn report_view(report: &plan::Report, text: Option<usize>, output: usize) -> serde_json::Value {
    let mut value =
        json!({ "status": report_text(&report.status, 32, false), "at": report.at, "data_notice": REPORT_DATA_NOTICE });
    let summary = report_text(&report.summary, usize::MAX, false);
    value["summary"] = json!(text.map_or_else(|| summary.clone(), |max| clip(&summary, max)));
    if !report.checks.is_empty() {
        let bounded = report.clone().bounded();
        let field_limit = text.unwrap_or(240).min(240);
        value["checks"] = json!(
            bounded
                .checks
                .iter()
                .map(|check| json!({
                    "check": clip(&check.check, field_limit.min(120)),
                    "command": clip(&check.command, field_limit),
                    "exit": check.exit,
                    // Compact evidence retains the end, where a runner normally prints its result.
                    "output_tail": report_text(&check.output_tail, output, true),
                }))
                .collect::<Vec<_>>()
        );
        if report.checks.len() > MAX_REPORT_CHECKS {
            value["checks_omitted"] = json!(report.checks.len() - MAX_REPORT_CHECKS);
        }
    }
    value
}

/// The newest notes a task's full view keeps; older ones are counted.
const MAX_TASK_NOTES_CHARS: usize = 16_000;
/// A task's full view, inside [`atomics_core::control::MAX_TOOL_TEXT`] with room for escapes.
const MAX_TASK_VIEW_CHARS: usize = 48_000;
const _: () = assert!(MAX_TASK_VIEW_CHARS < atomics_core::control::MAX_TOOL_TEXT);
/// Board tickets the compact view lists.
const MAX_BOARD_ITEMS: usize = 100;

/// How much a compact task carries. Plans that do not fit get shorter texts, then fewer
/// details for tasks that need no attention, then only names.
#[derive(Clone, Copy)]
struct Detail {
    /// Characters of the latest note and of the report summary.
    text: usize,
    /// Planned and done tasks show only their place in the plan.
    quiet_tasks_brief: bool,
    /// Every task shows only its place in the plan.
    all_brief: bool,
}

const LEVELS: [Detail; 5] = [
    Detail { text: 400, quiet_tasks_brief: false, all_brief: false },
    Detail { text: 150, quiet_tasks_brief: false, all_brief: false },
    Detail { text: 150, quiet_tasks_brief: true, all_brief: false },
    Detail { text: 60, quiet_tasks_brief: true, all_brief: false },
    Detail { text: 0, quiet_tasks_brief: true, all_brief: true },
];

impl State {
    /// A task as stored, with its integration verification, as the full plan shows it.
    pub(super) fn task_json(&self, index: usize, task: &Task) -> serde_json::Value {
        let mut value = serde_json::to_value(task).unwrap_or_default();
        if let Some(report) = &task.report {
            value["report"] = report_view(report, None, 240);
        }
        if task.integrated.is_some() {
            value["integration_verified"] = json!(task.integration_verified);
        }
        if let Some(note) = self.integration_check_note(index, task) {
            value["integration_check"] = json!(note);
        }
        value
    }

    /// `plan_get` with a plan: per task its status, dependencies, session, branch, latest note and
    /// report summary, cut until the whole reads within [`MAX_PLAN_VIEW_CHARS`].
    pub(super) fn plan_view(&self, index: usize) -> serde_json::Value {
        let plan = &self.plans[index];
        let mut value = json!({
            "id": plan.id,
            "status": plan.status,
            "goal": clip(&plan.goal, 1000),
            "project": plan.project,
            "approved": plan.approved,
            "integration": plan.integration_branch(),
            "workflow": self.plan_workflow(plan),
            "ready": plan.ready().iter().map(|t| &t.id).collect::<Vec<_>>(),
            "detail": "Long texts are cut here. Pass task to read one task in full: brief, acceptance, report and notes.",
        });
        if let Some(ticket) = &plan.ticket {
            let mut ticket = json!(ticket);
            clip_json(&mut ticket, 300, 20);
            value["ticket"] = ticket;
            value["ticket_mode"] = json!(plan.ticket_mode);
        }
        if !plan.stages.is_empty() {
            let recent = &plan.stages[plan.stages.len().saturating_sub(5)..];
            let mut stages = json!(recent);
            clip_json(&mut stages, 200, 5);
            value["stages"] = stages;
        }
        if let Some(note) = self.remote_note(&plan.id) {
            value["remote"] = json!(clip(&note, 500));
        }
        let problems = super::plans::cycle_problems(plan);
        if !problems.is_empty() {
            value["problems"] = json!(clip_list(&problems, 300, 10));
        }
        if let Some(board) = self.ticket_summary(&plan.id).and_then(|s| s.get("board").cloned()) {
            let items = board.as_array().map(Vec::as_slice).unwrap_or_default();
            let mut shown: Vec<_> = items
                .iter()
                .take(MAX_BOARD_ITEMS)
                .map(|item| {
                    let ticket = &item["ticket"];
                    json!({
                        "ticket": {
                            "key": ticket["key"].as_str().or(ticket["id"].as_str()),
                            "state": ticket["state"],
                            "title": ticket["title"].as_str().map(|title| clip(title, 80)),
                        },
                        "task": item["task"],
                    })
                })
                .collect();
            if items.len() > MAX_BOARD_ITEMS {
                shown.push(json!(format!("[{} more]", items.len() - MAX_BOARD_ITEMS)));
            }
            value["ticket_board"] = json!(shown);
        }
        for detail in LEVELS {
            value["tasks"] =
                json!(plan.tasks.iter().map(|task| self.compact_task(index, task, detail)).collect::<Vec<_>>());
            if shown_len(&value) <= MAX_PLAN_VIEW_CHARS {
                return value;
            }
        }
        // Still too long: as many tasks as fit, then the rest by name and status.
        let brief = LEVELS[LEVELS.len() - 1];
        let tasks: Vec<_> = plan.tasks.iter().map(|task| self.compact_task(index, task, brief)).collect();
        let rest = |count: usize| {
            plan.tasks[count..].iter().map(|task| format!("{} ({})", task.id, task.status.label())).collect::<Vec<_>>()
        };
        let fits = |count: usize, value: &mut serde_json::Value| {
            value["tasks"] = json!(tasks[..count]);
            value["more_tasks"] = json!(rest(count));
            shown_len(value) <= MAX_PLAN_VIEW_CHARS
        };
        let (mut low, mut high) = (0, tasks.len());
        while low < high {
            let middle = (low + high).div_ceil(2);
            if fits(middle, &mut value) { low = middle } else { high = middle - 1 }
        }
        fits(low, &mut value);
        value
    }

    fn compact_task(&self, index: usize, task: &Task, detail: Detail) -> serde_json::Value {
        let quiet = matches!(task.status, TaskStatus::Planned | TaskStatus::Done);
        let brief = detail.all_brief || detail.quiet_tasks_brief && quiet;
        let mut value = json!({
            "id": task.id,
            "title": clip(&task.title, if brief { 60 } else { 120 }),
            "status": task.status,
            "depends_on": task.depends_on,
            "session": task.session,
            "branch": task.branch,
            "notes": task.notes.len(),
        });
        if !brief && let Some(reason) = &task.reviewer_reason {
            value["reviewer_reason"] = json!(reason);
        }
        if let Some(isolation) = task.isolation {
            value["isolation"] = json!(isolation);
        }
        if let Some(into) = &task.integrated {
            value["integrated"] = json!(into);
            value["integration_verified"] = json!(task.integration_verified);
        }
        if let Some(note) = self.integration_check_note(index, task) {
            value["integration_check"] = json!(clip(&note, 200));
        }
        if let Some(alert) = &task.ref_alert {
            value["ref_alert"] = json!(clip(alert, 200));
        }
        if let Some(report) = &task.report {
            value["report"] = if brief {
                json!({ "status": report.status, "at": report.at, "checks_count": report.checks.len(), "data_notice": REPORT_DATA_NOTICE })
            } else {
                report_view(report, Some(detail.text), detail.text.min(240))
            };
        }
        if brief {
            return value;
        }
        if let Some(note) = task.notes.last() {
            value["note"] = json!(clip(note, detail.text));
        }
        if let Some(ticket) = &task.ticket {
            value["ticket"] = json!({
                "key": ticket.label(),
                "state": ticket.state,
                "error": ticket.error.as_deref().map(|error| clip(error, 200)),
            });
        }
        if let Some(stage) = task.stages.last() {
            value["stage"] = json!(stage.label());
        }
        value
    }

    /// `plan_get` with a plan and a task: the task in full, with its newest notes up to
    /// [`MAX_TASK_NOTES_CHARS`] and how many older ones were left out. A task too long for the
    /// client keeps fewer notes, then has its brief and report cut, pointing at the plan file.
    pub(super) fn task_view(&self, index: usize, id: &str) -> Result<serde_json::Value, String> {
        let plan = &self.plans[index];
        let Some(task) = plan.tasks.iter().find(|task| task.id == id) else {
            let ids = plan.tasks.iter().map(|task| task.id.as_str()).collect::<Vec<_>>().join(", ");
            return Err(format!("No task {id} in plan {}. Its tasks: {ids}", plan.id));
        };
        let mut value = self.task_json(index, task);
        value["ready"] = json!(plan.ready().iter().any(|ready| ready.id == task.id));
        let mut view = json!({ "plan": plan.id, "status": plan.status, "goal": clip(&plan.goal, 1000), "task": value });
        let (mut kept, mut size) = (0, 0);
        for note in task.notes.iter().rev() {
            size += note.chars().count();
            if kept > 0 && size > MAX_TASK_NOTES_CHARS {
                break;
            }
            kept += 1;
        }
        let keep_notes = |view: &mut serde_json::Value, kept: usize| {
            let omitted = task.notes.len() - kept;
            view["task"]["notes"] = json!(task.notes[omitted..]);
            if omitted > 0 {
                view["task"]["notes_omitted"] = json!(omitted);
            }
        };
        keep_notes(&mut view, kept);
        while kept > 1 && shown_len(&view) > MAX_TASK_VIEW_CHARS {
            kept /= 2;
            keep_notes(&mut view, kept);
        }
        if shown_len(&view) > MAX_TASK_VIEW_CHARS {
            let file = paths::atomics_home().join("plans").join(format!("{}.json", plan.id));
            view["task"]["cut"] = json!(format!(
                "Too long for one tool answer: the brief and report are cut. {} has them in full.",
                file.display()
            ));
            // The brief and the report summary share what is left; a short one keeps its length.
            let brief = task.brief.chars().count();
            let summary = task.report.as_ref().map_or(0, |report| report.summary.chars().count());
            let mut room = brief + summary;
            // Sizes count bytes and cuts count characters: cut again until it fits.
            for _ in 0..4 {
                let size = shown_len(&view);
                if size <= MAX_TASK_VIEW_CHARS {
                    break;
                }
                room = room.saturating_sub(size - MAX_TASK_VIEW_CHARS + 64);
                let (brief_max, summary_max) = if brief <= room / 2 {
                    (brief, room - brief)
                } else if summary <= room / 2 {
                    (room - summary, summary)
                } else {
                    (room / 2, room - room / 2)
                };
                view["task"]["brief"] = json!(clip(&task.brief, brief_max));
                if let Some(report) = &task.report {
                    view["task"]["report"]["summary"] = json!(clip(&report.summary, summary_max));
                }
            }
        }
        Ok(view)
    }
}

#[cfg(test)]
mod tests {
    use atomics_core::control::{
        DEFAULT_WAIT_SECS, MAX_PLAN_VIEW_CHARS, MAX_TOOL_TEXT, MAX_WAIT_CHARS, MAX_WAIT_SECS, shown_len,
    };
    use atomics_core::plan::Report;

    use super::*;
    use crate::app::tests::{request, session, with_app};

    #[test]
    fn reports_store_checks_and_plan_get_returns_bounded_evidence_as_data() {
        with_app(
            "app::plan_read::tests::reports_store_checks_and_plan_get_returns_bounded_evidence_as_data",
            |app, _| {
                let response = request(app, json!({"cmd":"plan_set", "goal":"Evidence", "tasks":[{"id":"a"}]}));
                assert!(response.ok, "{response:?}");
                let plan = response.result["plan"].as_str().unwrap().to_string();
                let index = app.runtime.plans.iter().position(|p| p.id == plan).unwrap();
                app.runtime.plans[index].tasks[0].session = Some("worker".into());
                let check = json!({"check":"acceptance check", "command":"cargo test", "exit":0,
                "output_tail":format!("{}\npassed\u{1b}", "界".repeat(4000))});
                let response = request(
                    app,
                    json!({"cmd":"report", "pane":"worker", "status":"done",
                "summary":"Tests passed\u{0}", "checks":vec![check; 20]}),
                );
                assert!(response.ok, "{response:?}");
                let task = &app.runtime.plans[index].tasks[0];
                assert_eq!(task.status, TaskStatus::Review);
                let report = task.report.as_ref().unwrap();
                assert_eq!(report.summary, "Tests passed");
                assert_eq!(report.checks.len(), MAX_REPORT_CHECKS);
                assert_eq!(report.checks[0].output_tail.chars().count(), plan::MAX_CHECK_OUTPUT);
                let stored = Plan::load(&paths::atomics_home().join("plans").join(format!("{plan}.json"))).unwrap();
                assert_eq!(stored.tasks[0].report, task.report);
                for args in [json!({"cmd":"plan_get", "plan":plan}), json!({"cmd":"plan_get", "plan":plan, "task":"a"})]
                {
                    let view = request(app, args);
                    assert!(view.ok, "{view:?}");
                    assert!(shown_len(&view.result) <= MAX_PLAN_VIEW_CHARS);
                    let task =
                        if view.result.get("task").is_some() { &view.result["task"] } else { &view.result["tasks"][0] };
                    let report = &task["report"];
                    assert_eq!(report["data_notice"], REPORT_DATA_NOTICE);
                    assert_eq!(report["checks"][0]["command"], "cargo test");
                    assert_eq!(report["checks"][0]["exit"], 0);
                    let tail = report["checks"][0]["output_tail"].as_str().unwrap();
                    assert!(tail.ends_with("\npassed"));
                    assert!(tail.chars().count() <= 240);
                }

                // Legacy calls without checks remain valid.
                assert!(
                    request(
                        app,
                        json!({"cmd":"report", "pane":"worker", "status":"progress", "summary":"still working"})
                    )
                    .ok
                );
                assert!(app.runtime.plans[index].tasks[0].report.as_ref().unwrap().checks.is_empty());
            },
        );
    }

    /// A plan of `count` tasks with long titles, briefs, reports and note histories, in every status.
    fn large_plan(app: &mut Desktop<'_>, count: usize, brief_words: usize) -> (String, usize) {
        let tasks: Vec<_> = (0..count)
            .map(|i| {
                json!({
                    "id": format!("t{i}"),
                    "title": format!("Task {i} {}", "with a long title ".repeat(20)),
                    "brief": format!("brief {i} {}", "context ".repeat(brief_words)),
                    "acceptance": ["the tests pass ".repeat(100)],
                    "depends_on": if i > 0 { vec![format!("t{}", i - 1)] } else { vec![] },
                    "branch": format!("atomics/a-fairly-long-branch-name-for-task-{i}"),
                })
            })
            .collect();
        let plan = request(app, json!({"cmd":"plan_set", "goal":"Large", "tasks":tasks})).result["plan"]
            .as_str()
            .unwrap()
            .to_string();
        let index = app.runtime.plans.iter().position(|p| p.id == plan).unwrap();
        for (i, task) in app.runtime.plans[index].tasks.iter_mut().enumerate() {
            task.status = TaskStatus::ALL[i % TaskStatus::ALL.len()];
            task.session = Some(format!("{i:08}-0000-4000-8000-000000000000"));
            task.notes = (0..200).map(|n| format!("note {n} {}", "detail ".repeat(70))).collect();
            task.report = Some(Report {
                status: "done".into(),
                summary: format!("summary {i} {}", "result ".repeat(4000)),
                at: 1,
                checks: Vec::new(),
            });
        }
        (plan, index)
    }

    #[test]
    fn plan_get_stays_compact_for_a_large_plan_and_reads_one_task_in_full() {
        with_app(
            "app::plan_read::tests::plan_get_stays_compact_for_a_large_plan_and_reads_one_task_in_full",
            |app, _| {
                let (plan, index) = large_plan(app, 20, 2500);
                let stored = shown_len(&app.runtime.plan_json(index));
                assert!(stored > 30 * MAX_PLAN_VIEW_CHARS, "the stored plan is far over the limit: {stored}");

                let view = request(app, json!({"cmd":"plan_get", "plan":plan}));
                assert!(view.ok, "{view:?}");
                let size = shown_len(&view.result);
                assert!(size <= MAX_PLAN_VIEW_CHARS, "{size}");
                assert_eq!(view.result["goal"], "Large");
                assert!(view.result["detail"].as_str().unwrap().contains("Pass task"));
                let tasks = view.result["tasks"].as_array().unwrap();
                let stored = &app.runtime.plans[index].tasks;
                assert_eq!(tasks.len(), stored.len(), "every task is listed, the final review too");
                assert!(view.result.get("more_tasks").is_none());
                for (i, (task, stored)) in tasks.iter().zip(stored).enumerate() {
                    assert_eq!(task["id"], stored.id);
                    assert_eq!(task["status"], stored.status.label());
                    assert_eq!(task["session"], format!("{i:08}-0000-4000-8000-000000000000"));
                    assert_eq!(task["branch"], json!(stored.branch));
                    assert_eq!(task["depends_on"], json!(stored.depends_on));
                    assert_eq!(task["notes"], 200);
                    assert_eq!(task["report"]["status"], "done");
                    assert!(task.get("brief").is_none() && task.get("acceptance").is_none(), "{task}");
                    // The latest note and the report summary, cut.
                    let note = task["note"].as_str().unwrap();
                    assert!(note.starts_with("note 199 ") && note.ends_with("more characters]"), "{note}");
                    let summary = task["report"]["summary"].as_str().unwrap();
                    assert!(summary.starts_with(&format!("summary {i} ")) && summary.ends_with("more characters]"));
                }
                assert!(tasks[0]["title"].as_str().unwrap().ends_with("more characters]"), "{}", tasks[0]);

                // A task with a short report: everything in full, its newest notes up to their limit.
                app.runtime.plans[index].tasks[7].report.as_mut().unwrap().summary =
                    "Done: 3 files, tests pass.".into();
                let full = request(app, json!({"cmd":"plan_get", "plan":plan, "task":"t7"}));
                assert!(full.ok, "{full:?}");
                let task = &full.result["task"];
                let stored = &app.runtime.plans[index].tasks[7];
                assert_eq!(full.result["plan"], plan);
                assert_eq!(task["brief"], stored.brief, "the brief in full");
                assert_eq!(task["acceptance"], json!(stored.acceptance));
                assert_eq!(task["title"], stored.title);
                assert_eq!(task["status"], stored.status.label());
                assert_eq!(task["report"]["summary"], "Done: 3 files, tests pass.");
                assert!(task.get("cut").is_none());
                let notes = task["notes"].as_array().unwrap();
                let omitted = task["notes_omitted"].as_u64().unwrap() as usize;
                assert_eq!(omitted + notes.len(), 200);
                assert!(omitted > 0 && notes.len() > 1);
                assert_eq!(notes.last().unwrap(), &json!(stored.notes[199]), "the newest notes are kept");
                assert_eq!(notes[0], json!(stored.notes[omitted]));
                let kept: usize = notes.iter().map(|note| note.as_str().unwrap().chars().count()).sum();
                assert!(kept <= MAX_TASK_NOTES_CHARS, "{kept}");

                // One too long for a tool answer keeps fewer notes, then cuts its report, and says where it all is.
                let full = request(app, json!({"cmd":"plan_get", "plan":plan, "task":"t8"}));
                let size = shown_len(&full.result);
                assert!(size <= MAX_TASK_VIEW_CHARS, "{size}");
                let task = &full.result["task"];
                let stored = &app.runtime.plans[index].tasks[8];
                assert_eq!(task["brief"], stored.brief, "the shorter text stays whole");
                assert_eq!(task["status"], stored.status.label());
                assert!(task["report"]["summary"].as_str().unwrap().ends_with("more characters]"));
                assert_eq!(task["notes"], json!([stored.notes[199]]), "only the newest note");
                assert_eq!(task["notes_omitted"], 199);
                let cut = task["cut"].as_str().unwrap();
                assert!(cut.contains(&format!("plans/{plan}.json")), "{cut}");

                let missing = request(app, json!({"cmd":"plan_get", "plan":plan, "task":"nope"}));
                let error = missing.error.unwrap();
                assert!(error.contains("No task nope") && error.contains("t0, t1"), "{error}");
                let unplanned = request(app, json!({"cmd":"plan_get", "task":"t1"}));
                assert!(unplanned.error.unwrap().contains("Pass plan with task"));
                let summary = request(app, json!({"cmd":"plan_get"}));
                assert_eq!(summary.result["plans"][0]["id"], plan, "without plan, the summaries");
                assert_eq!(summary.result["plans"][0]["total"], app.runtime.plans[index].tasks.len());
            },
        );
    }

    #[test]
    fn plan_get_names_the_tasks_that_do_not_fit_by_id_and_status() {
        with_app("app::plan_read::tests::plan_get_names_the_tasks_that_do_not_fit_by_id_and_status", |app, _| {
            let (plan, index) = large_plan(app, atomics_core::ops::MAX_TASKS - 1, 10);
            let view = request(app, json!({"cmd":"plan_get", "plan":plan}));
            assert!(shown_len(&view.result) <= MAX_PLAN_VIEW_CHARS, "{}", shown_len(&view.result));
            let tasks = view.result["tasks"].as_array().unwrap();
            let more = view.result["more_tasks"].as_array().unwrap();
            assert!(!tasks.is_empty() && !more.is_empty());
            assert_eq!(tasks.len() + more.len(), app.runtime.plans[index].tasks.len());
            let review = app.runtime.plans[index].tasks.last().unwrap();
            assert_eq!(more.last().unwrap(), &json!(format!("{} ({})", review.id, review.status.label())));
            for (i, task) in tasks.iter().enumerate() {
                assert_eq!(task["id"], format!("t{i}"));
                assert!(task.get("note").is_none(), "only the task's place in the plan: {task}");
            }
            let first = tasks.len();
            assert_eq!(more[0], format!("t{first} ({})", TaskStatus::ALL[first % TaskStatus::ALL.len()].label()));
        });
    }

    #[test]
    fn plan_set_refusals_and_a_stored_plans_cycles_read_bounded() {
        with_app("app::plan_read::tests::plan_set_refusals_and_a_stored_plans_cycles_read_bounded", |app, _| {
            // Thirty tasks that each wait for themselves: thirty refusals, of which the first twenty are listed.
            let looped: Vec<_> =
                (0..30).map(|i| json!({"id":format!("t{i}"), "depends_on":[format!("t{i}")]})).collect();
            let refused = request(app, json!({"cmd":"plan_set", "goal":"Loops", "tasks":looped}));
            let error = refused.error.unwrap();
            let lines: Vec<_> = error.lines().filter(|line| line.starts_with("- ")).collect();
            assert_eq!(lines.len(), 21, "{error}");
            assert_eq!(
                lines[0],
                "- Dependency cycle t0 → t0: each task waits for the next, so none of them can ever start."
            );
            assert_eq!(lines[20], "- [10 more]");
            assert!(app.runtime.plans.iter().all(|plan| plan.goal != "Loops"), "nothing was saved");

            // A plan saved before plan_set refused cycles shows them in plan_get, bounded the same way.
            let plan = request(app, json!({"cmd":"plan_set", "goal":"Older", "tasks":[{"id":"a"}, {"id":"b"}]})).result
                ["plan"]
                .as_str()
                .unwrap()
                .to_string();
            let view = request(app, json!({"cmd":"plan_get", "plan":plan}));
            assert!(view.result.get("problems").is_none(), "{}", view.result);
            let index = app.runtime.plans.iter().position(|p| p.id == plan).unwrap();
            let tasks = &mut app.runtime.plans[index].tasks;
            tasks[0].depends_on = vec!["b".into()];
            tasks[1].depends_on = vec!["a".into()];
            for i in 0..20 {
                tasks.push(Task { id: format!("s{i}"), depends_on: vec![format!("s{i}")], ..Task::default() });
            }
            let view = request(app, json!({"cmd":"plan_get", "plan":plan}));
            let problems = view.result["problems"].as_array().unwrap();
            assert_eq!(problems.len(), 11, "{problems:?}");
            assert!(problems[0].as_str().unwrap().starts_with("Dependency cycle a → b → a: "), "{problems:?}");
            assert_eq!(problems[10], "[11 more]");
            assert!(shown_len(&view.result) <= MAX_PLAN_VIEW_CHARS);
        });
    }

    #[test]
    fn wait_answers_in_parts_that_fit_and_its_cursor_reads_on() {
        with_app("app::plan_read::tests::wait_answers_in_parts_that_fit_and_its_cursor_reads_on", |app, _| {
            let mut after = app.runtime.event_seq;
            for i in 0..30 {
                let summary = format!("report {i} {}", "word ".repeat(4000));
                assert!(
                    request(app, json!({"cmd":"report", "pane":"gone", "status":"progress", "summary":summary})).ok
                );
            }
            let (mut seen, mut answers) = (Vec::new(), 0);
            loop {
                let answer = request(app, json!({"cmd":"wait", "after":after, "statuses":["reported"]}));
                let events = answer.result["events"].as_array().unwrap();
                assert!(!events.is_empty());
                assert!(shown_len(&json!(events)) <= MAX_WAIT_CHARS, "{}", shown_len(&json!(events)));
                for event in events {
                    let summary = event["report"]["summary"].as_str().unwrap();
                    assert!(summary.chars().count() < 8100 && summary.ends_with("more characters]"));
                    seen.push(summary.split(' ').nth(1).unwrap().parse::<usize>().unwrap());
                }
                answers += 1;
                after = answer.result["cursor"].as_u64().unwrap();
                if answer.result.get("more").is_none() {
                    break;
                }
            }
            assert!(answers > 1, "the events took more than one answer");
            assert_eq!(seen, (0..30).collect::<Vec<_>>(), "each event once, in order");
            assert_eq!(after, app.runtime.event_seq);

            // However long the chief asks to wait, the answer comes within the client's limit.
            let mut deadline = |timeout: Option<u64>| {
                let (tx, _rx) = std::sync::mpsc::channel();
                app.runtime.control(
                    ControlRequest::Wait {
                        sessions: vec![],
                        statuses: vec!["heartbeat".into()],
                        after: Some(u64::MAX),
                        timeout_secs: timeout,
                    },
                    tx,
                );
                let waiter = app.runtime.waiters.pop().expect("a waiting request");
                waiter.deadline.saturating_duration_since(Instant::now())
            };
            let longest = deadline(Some(280));
            assert!(longest <= Duration::from_secs(MAX_WAIT_SECS), "{longest:?}");
            assert!(longest > Duration::from_secs(MAX_WAIT_SECS - 5), "{longest:?}");
            let default = deadline(None);
            assert!(
                default <= Duration::from_secs(DEFAULT_WAIT_SECS)
                    && default > Duration::from_secs(DEFAULT_WAIT_SECS - 5)
            );
        });
    }

    #[test]
    fn list_result_and_read_cut_long_prompts_and_messages() {
        with_app("app::plan_read::tests::list_result_and_read_cut_long_prompts_and_messages", |app, _| {
            let root = app.runtime.launch_dir.clone();
            app.runtime.sessions = (0..30)
                .map(|i| {
                    let mut session = session(&format!("s{i}"), AgentKind::Claude, AgentStatus::Idle, &root);
                    session.last_prompt = Some(format!("brief {i} {}", "context ".repeat(20_000)));
                    session.last_message = Some(format!("message {i} {}", "result ".repeat(20_000)));
                    session
                })
                .collect();
            let list = request(app, json!({"cmd":"list"}));
            assert!(shown_len(&list.result) < MAX_TOOL_TEXT, "{}", shown_len(&list.result));
            for (i, session) in list.result["sessions"].as_array().unwrap().iter().enumerate() {
                let prompt = session["last_prompt"].as_str().unwrap();
                assert!(prompt.starts_with(&format!("brief {i} ")) && prompt.chars().count() < 340, "{prompt}");
            }
            let result = request(app, json!({"cmd":"result", "session":"s3"}));
            assert!(shown_len(&result.result) < MAX_TOOL_TEXT, "{}", shown_len(&result.result));
            let message = result.result["last_message"].as_str().unwrap();
            assert!(message.starts_with("message 3 ") && message.ends_with("more characters]"));
            assert!(result.result["last_prompt"].as_str().unwrap().ends_with("more characters]"));
            // Without a terminal, read shows the same texts, cut the same way.
            let read = request(app, json!({"cmd":"read", "session":"s4", "lines":100_000}));
            assert!(shown_len(&read.result) < MAX_TOOL_TEXT, "{}", shown_len(&read.result));
            assert!(read.result["last_message"].as_str().unwrap().starts_with("message 4 "));
        });
    }
}
