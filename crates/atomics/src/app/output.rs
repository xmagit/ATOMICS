//! What boards and chats produced, recorded where it came from.
use super::*;
use atomics_core::output::{self, Made, Output};

/// What a report names, checked: its task is the reporting session's.
pub(super) fn reported_output(list: Vec<Output>) -> Result<Vec<Output>, String> {
    if list.len() > output::MAX_REPORTED {
        return Err(format!("Report at most {} outputs", output::MAX_REPORTED));
    }
    list.iter().map(|item| Output::checked(&item.url, &item.title)).collect()
}

impl State {
    pub(super) fn record_reported(&mut self, pane: Option<&str>, list: Vec<Output>) {
        let Some(pane) = pane.filter(|_| !list.is_empty()) else { return };
        let Some(index) = self.plans.iter().position(|plan| plan.contains_session(pane)) else { return };
        self.record_output(index, Some(pane), list);
    }

    /// Records the pull request a session made: on its board, else on its chat. A chief's is on the board whose branch
    /// it is of, then the board its turn is for, then its thread's only open one.
    pub(super) fn record_made(&mut self, index: usize, conversation: &str, made: &Made) {
        let Ok(found) = Output::checked(&made.url, "") else { return };
        let session = &self.sessions[index];
        let id = session.id.clone();
        if !session.chief {
            if let Some(plan) = self.plans.iter().position(|plan| plan.contains_session(&id)) {
                self.record_output(plan, Some(&id), vec![found]);
            }
            return;
        }
        let of_thread =
            |plan: &Plan| plan.is_open() && !plan.id.is_empty() && self.thread_of_plan(plan) == session.thread;
        let of_branch = made.head.as_ref().and_then(|head| {
            let mut plans =
                self.plans.iter().enumerate().filter(|(_, plan)| !plan.id.is_empty() && plan.works_on(head));
            plans.next().filter(|_| plans.next().is_none())
        });
        let of_turn = || {
            let (root, board) = session.simple_turn_plan.as_ref()?;
            self.plans.iter().enumerate().find(|(_, plan)| root == conversation && &plan.id == board)
        };
        let only = || {
            let mut plans = self.plans.iter().enumerate().filter(|(_, plan)| of_thread(plan));
            plans.next().filter(|_| plans.next().is_none())
        };
        let chat = session.simple_chat.as_ref().filter(|(root, _)| root == conversation).map(|(_, chat)| chat.clone());
        let board = of_branch.or_else(of_turn).map(|(plan, _)| plan);
        // A chat's own work is the chat's, though its session has a board too.
        let board =
            board.or_else(|| only().filter(|_| chat.is_none() || !session.thread.is_empty()).map(|(plan, _)| plan));
        if let Some(plan) = board {
            self.record_output(plan, None, vec![found]);
            return;
        }
        let chat = chat.unwrap_or_else(|| conversation.to_owned());
        let Some(known) = self.store.chief_history.iter_mut().find(|c| c.conversation == chat) else { return };
        if output::add(&mut known.output, Output { at: atomics_core::store::now(), ..found })
            && let Err(error) = self.store.save_later(&paths::store_file())
        {
            self.toast(format!("Could not save what the chat made: {error}"));
        }
    }

    fn record_output(&mut self, plan: usize, pane: Option<&str>, list: Vec<Output>) {
        let at = atomics_core::store::now();
        let task = pane.and_then(|pane| {
            let mut tasks = self.plans[plan].tasks.iter();
            tasks.find(|t| t.session.as_deref() == Some(pane)).map(|t| t.id.clone())
        });
        let mut added = false;
        for item in list {
            added |= output::add(&mut self.plans[plan].output, Output { task: task.clone(), at, ..item });
        }
        if added {
            self.save_plan(plan);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{session, with_app};
    use super::*;

    const CHAT: &str = "chat-0123456789abcdef0123456789abcdef";

    fn board(id: &str, agent: &str, branch: &str) -> Plan {
        Plan {
            id: id.into(),
            tasks: vec![Task {
                id: "t1".into(),
                session: Some(agent.into()),
                branch: Some(branch.into()),
                ..Task::default()
            }],
            ..Plan::default()
        }
    }

    fn made(number: u64, head: Option<&str>) -> Made {
        Made { url: format!("https://github.com/o/r/pull/{number}"), head: head.map(str::to_owned) }
    }

    #[test]
    fn what_a_session_made_is_on_its_board_and_a_chats_on_the_chat() {
        with_app("app::output::tests::what_a_session_made_is_on_its_board_and_a_chats_on_the_chat", |app, _| {
            let state = &mut *app.runtime;
            let dir = state.launch_dir.clone();
            let mut chief = session("chief", AgentKind::Claude, AgentStatus::Working, &dir);
            chief.chief = true;
            state.sessions = vec![chief, session("a", AgentKind::Claude, AgentStatus::Working, &dir)];
            state.sessions.push(session("loose", AgentKind::Claude, AgentStatus::Working, &dir));
            state.plans = vec![board("one", "a", "atomics/one-t1"), board("two", "b", "atomics/two-t1")];
            state.store.chief_history.push(atomics_core::projection::ChiefConversation {
                conversation: CHAT.into(),
                title: String::new(),
                plan: None,
                thread: None,
                agent: Default::default(),
                turns: vec![],
                output: vec![],
            });
            let listed = |state: &State| {
                let names = |list: &[Output]| list.iter().filter_map(|o| Some(o.pull_request()?.1)).collect::<Vec<_>>();
                let plans = state.plans.iter().map(|plan| names(&plan.outputs())).collect::<Vec<_>>();
                (plans, names(&state.store.chief_history[0].output))
            };
            // An agent's is its task's; a loose one's is nobody's.
            state.record_made(1, "root-a", &made(1, None));
            state.record_made(1, "root-a", &made(1, None));
            state.record_made(2, "root-x", &made(9, None));
            assert_eq!(listed(state), (vec![vec![1], vec![]], vec![]));
            assert_eq!(state.plans[0].output[0].task.as_deref(), Some("t1"));
            // A chief's is of the branch it names, then of the board its turn is for.
            state.record_made(0, "root", &made(2, Some("atomics/two-t1")));
            state.sessions[0].simple_turn_plan = Some(("root".into(), "one".into()));
            state.record_made(0, "root", &made(3, None));
            // With several boards and no turn of one, it is not guessed; a chat's is the chat's.
            state.sessions[0].simple_turn_plan = None;
            state.record_made(0, "root", &made(8, None));
            state.sessions[0].simple_chat = Some(("root".into(), CHAT.into()));
            state.record_made(0, "root", &made(4, None));
            assert_eq!(listed(state), (vec![vec![1, 3], vec![2]], vec![4]));
            // A thread of its own with one board: what it makes is the board's.
            state.sessions[0].thread = CHAT.into();
            state.plans[1].thread = CHAT.into();
            state.record_made(0, "root", &made(5, None));
            assert_eq!(listed(state), (vec![vec![1, 3], vec![2, 5]], vec![4]));
            // What a task and a publication name is listed once, with the rest.
            let pr = |number| atomics_core::plan::PullRequest {
                url: format!("https://github.com/o/r/pull/{number}"),
                number,
                head: "h".into(),
                base: "main".into(),
            };
            state.plans[0].tasks[0].pull_request = Some(pr(1));
            state.plans[0].pull_request = Some(pr(6));
            assert_eq!(listed(state).0[0], [1, 3, 6]);
        });
    }

    #[test]
    fn a_report_names_what_its_task_produced() {
        with_app("app::output::tests::a_report_names_what_its_task_produced", |app, _| {
            let state = &mut *app.runtime;
            let dir = state.launch_dir.clone();
            state.sessions = vec![session("a", AgentKind::Claude, AgentStatus::Working, &dir)];
            state.plans = vec![board("one", "a", "atomics/one-t1")];
            let report = |state: &mut State, list: &[(&str, &str)]| {
                let (reply, answer) = std::sync::mpsc::channel();
                let output = list.iter().map(|(url, title)| Output {
                    url: url.to_string(),
                    title: title.to_string(),
                    task: Some("another".into()),
                    at: 1,
                });
                let request = ControlRequest::Report {
                    pane: Some("a".into()),
                    status: "progress".into(),
                    summary: "So far".into(),
                    checks: vec![],
                    pull_request: None,
                    output: output.collect(),
                };
                state.control(request, reply);
                match answer.try_recv() {
                    Ok(ControlOutcome::Done(response)) => response.ok,
                    _ => panic!("no answer"),
                }
            };
            assert!(report(state, &[("https://a.example/report", "Benchmark")]));
            let recorded = &state.plans[0].output[0];
            assert_eq!((recorded.title.as_str(), recorded.task.as_deref()), ("Benchmark", Some("t1")));
            assert!(recorded.at > 1);
            assert!(!report(state, &[("https://a.example/x", ""), ("file:///etc/passwd", "")]));
            assert_eq!(state.plans[0].output.len(), 1, "a refused report records nothing");
            let many = vec![("https://a.example/x", ""); output::MAX_REPORTED + 1];
            assert!(!report(state, &many));
        });
    }
}
