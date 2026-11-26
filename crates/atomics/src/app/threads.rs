//! The chief's threads. Each is a conversation of its own, with a chief session that runs while it is used, beside the
//! others. A board belongs to the thread it was planned in, and its agents report there. The thread a client opened
//! last is in front: what the window shows of "the chief", and where a message without a name for its thread goes.
use super::*;
use atomics_core::projection::ChiefSelection;

/// How long a chief rests, out of view and with no work open, before its session is closed. Its thread keeps the
/// conversation and continues it when it is written to.
const SLEEP_AFTER: Duration = Duration::from_secs(30 * 60);
/// How many chats keep a chief picked for them before they were first written to.
const MAX_CHOICES: usize = 32;

impl State {
    /// The chief session of `thread`, if one runs.
    pub(super) fn chief_of(&self, thread: &str) -> Option<usize> {
        self.sessions.iter().position(|s| s.chief && s.thread == thread && !self.is_chief_replacement(&s.id))
    }

    /// The chief in front, if it runs.
    pub(super) fn chief_index(&self) -> Option<usize> {
        self.chief_of(&self.store.active_chief_chat)
    }

    pub(super) fn chief(&self) -> Option<&Session> {
        self.chief_index().map(|index| &self.sessions[index])
    }

    /// The chief session a control request came from, as the socket knows it.
    pub(super) fn calling_chief(&self) -> Option<&Session> {
        let pane = self.control_caller.as_deref()?;
        self.sessions.iter().find(|s| s.chief && s.id == pane)
    }

    fn live_chiefs(&self) -> impl Iterator<Item = &Session> {
        self.sessions.iter().filter(|s| s.current_chief() && !self.is_chief_replacement(&s.id))
    }

    /// The thread that manages `plan`. Plans from before threads had names belong to the chat they were planned in.
    pub(super) fn thread_of_plan<'a>(&'a self, plan: &'a Plan) -> &'a str {
        if !plan.thread.is_empty() || plan.id.is_empty() {
            return &plan.thread;
        }
        let chat = self.store.chief_history.iter().find(|chat| chat.plan.as_deref() == Some(&plan.id));
        chat.filter(|chat| atomics_core::trace::valid_chat_id(&chat.conversation)).map_or("", |chat| {
            chat.thread.as_deref().unwrap_or_else(|| {
                // Chats from before they had threads of their own were part of the workspace's.
                if self.store.chief_threads.contains_key(&chat.conversation) { &chat.conversation } else { "" }
            })
        })
    }

    /// Where what is nobody's in particular goes: the workspace's thread while its chief runs, else the one in front,
    /// else any.
    fn lead_thread(&self) -> Option<&str> {
        let live = |thread: &str| self.live_chiefs().find(|s| s.thread == thread).map(|s| s.thread.as_str());
        live("")
            .or_else(|| live(&self.store.active_chief_chat))
            .or_else(|| self.live_chiefs().next().map(|s| s.thread.as_str()))
    }

    /// The thread a board's news go to: its own while its chief runs, else the lead's, so that nothing goes unheard.
    fn hearing<'a>(&'a self, plan: &'a Plan) -> Option<&'a str> {
        let thread = self.thread_of_plan(plan);
        if self.live_chiefs().any(|s| s.thread == thread) { Some(thread) } else { self.lead_thread() }
    }

    /// Whether the chief of `thread` is told of `event`.
    pub(super) fn hears(&self, thread: &str, event: &StatusEvent) -> bool {
        // That a chief ended its turn is no news to itself, nor to another.
        if event.plan.is_none() && self.sessions.iter().any(|s| s.chief && s.id == event.session) {
            return false;
        }
        let plan = match &event.plan {
            Some(id) => self.plans.iter().find(|plan| &plan.id == id),
            None => self.plans.iter().find(|plan| plan.contains_session(&event.session)),
        };
        if event.findings.as_ref().is_some_and(|findings| {
            findings
                .iter()
                .any(|finding| self.plan_named(&finding.plan).and_then(|plan| self.hearing(plan)) == Some(thread))
        }) {
            return true;
        }
        plan.map_or_else(|| self.lead_thread(), |plan| self.hearing(plan)) == Some(thread)
    }

    /// Whether `session` is one of those `thread`'s chief is told of.
    pub(super) fn hears_session(&self, thread: &str, session: &Session) -> bool {
        if session.chief {
            return session.thread == thread;
        }
        let plan = self.plans.iter().find(|plan| plan.contains_session(&session.id));
        plan.map_or_else(|| self.lead_thread(), |plan| self.hearing(plan)) == Some(thread)
    }

    /// Whether `thread`'s chief is told of a heartbeat's `finding`: those about a board go to its own.
    pub(super) fn hears_finding(&self, thread: &str, finding: &atomics_core::heartbeat::Finding) -> bool {
        self.plan_named(&finding.plan).map_or_else(|| self.lead_thread(), |plan| self.hearing(plan)) == Some(thread)
    }

    fn plan_named(&self, id: &str) -> Option<&Plan> {
        self.plans.iter().find(|plan| !id.is_empty() && plan.id == id)
    }

    /// Whether `thread` has work open that its chief follows.
    pub(super) fn thread_works(&self, thread: &str) -> bool {
        let working = |plan: &&Plan| {
            plan.is_open()
                && plan.tasks.iter().any(|task| matches!(task.status, TaskStatus::Running | TaskStatus::Review))
        };
        self.plans.iter().filter(working).any(|plan| self.hearing(plan) == Some(thread))
            || self.lead_thread() == Some(thread)
                && self.sessions.iter().any(|s| {
                    !s.chief
                        && s.is_agent()
                        && !self.plans.iter().any(|plan| plan.is_open() && plan.contains_session(&s.id))
                })
    }

    /// Remembers the conversation each running chief is in, also after a `/clear`: its thread continues there.
    pub(super) fn record_threads(&mut self) {
        let mut changed = false;
        let started: Vec<_> = self.live_chiefs().map(|s| s.id.clone()).collect();
        for chief in started {
            self.chief_heard.entry(chief).or_insert(self.event_seq);
        }
        let preferred = self.preferred_chief();
        let running: Vec<_> = self
            .live_chiefs()
            .filter(|s| s.hooks_active)
            .filter_map(|s| {
                let root = s.claude_id.clone().filter(|root| !root.is_empty())?;
                Some((s.thread.clone(), s.chief_selection.clone().unwrap_or_else(|| preferred.clone()), root))
            })
            .collect();
        for (thread, choice, root) in running {
            // Its conversation is known now, with the provider and model it runs.
            changed |= self.store.chief_choices.remove(&thread).is_some();
            let pin = thread.is_empty().then(|| self.chief_resume(choice.agent)).flatten();
            let now = (choice, root);
            if self.store.chief_threads.get(&thread) != Some(&now) {
                if thread.is_empty() {
                    self.store.chief_resumed = pin;
                }
                self.store.chief_threads.insert(thread, now);
                changed = true;
            }
        }
        if changed && let Err(error) = self.store.save_later(&paths::store_file()) {
            self.toast(format!("Could not save the chief's threads: {error}"));
        }
    }

    /// What the next chief of `thread` runs and continues. `[chief] resume` starts the workspace's thread, and starts it
    /// again once when it is changed.
    pub(super) fn thread_start(&self, thread: &str) -> (atomics_core::projection::ChiefSelection, Option<String>) {
        let stored = self.store.chief_threads.get(thread).map(|(choice, root)| (choice.clone(), Some(root.clone())));
        let preferred = self.preferred_chief();
        if !thread.is_empty() {
            let chosen = self.store.chief_choices.get(thread).cloned();
            return stored.unwrap_or_else(|| (chosen.unwrap_or(preferred), None));
        }
        let pin = self.chief_resume(preferred.agent);
        stored
            .filter(|(choice, _)| choice.agent == preferred.agent && self.store.chief_resumed == pin)
            .map(|(_, root)| (preferred.clone(), root))
            .unwrap_or((preferred, pin))
    }

    /// A chief whose conversation could not be continued ended before it started: the next one starts anew.
    pub(super) fn forget_unstarted(&mut self, index: usize) {
        let session = &self.sessions[index];
        if !session.hooks_active
            && session.status != AgentStatus::Exited
            && self.store.chief_threads.get(&session.thread).map(|(_, root)| Some(root))
                == Some(session.claude_id.as_ref())
        {
            let thread = session.thread.clone();
            if thread.is_empty() {
                self.store.chief_resumed = self.chief_resume(self.preferred_chief().agent);
            }
            self.store.chief_threads.remove(&thread);
        }
    }

    /// The thread a saved chief comes back to. A record from before a chief had one is its conversation's.
    pub(super) fn thread_of_record(&self, record: &SessionRecord) -> String {
        let mut known = self.store.chief_threads.iter();
        let conversation = &record.claude_session_id;
        let old = record.chief && record.thread.is_empty() && !conversation.is_empty();
        old.then(|| known.find(|(thread, (_, root))| !thread.is_empty() && root == conversation))
            .flatten()
            .map_or_else(|| record.thread.clone(), |(thread, _)| thread.clone())
    }

    /// Picks the provider and model the chief of `thread` starts with, before it ever started: there is no conversation
    /// to continue, so this is no resume. Only what the host offers ([`Self::offered_chief`]).
    pub(super) fn choose_thread_chief(&mut self, thread: &str, choice: &ChiefSelection) -> Result<(), String> {
        if !atomics_core::trace::valid_chat_id(thread) {
            return Err("Only a new chat's chief is chosen here; change the workspace's from the model menu".into());
        }
        if self.chief_of(thread).is_some() || self.store.chief_threads.contains_key(thread) {
            return Err("This chat's chief has started; change it from the model menu".into());
        }
        self.offered_chief(choice)?;
        if self.store.chief_choices.get(thread) == Some(choice) {
            return Ok(());
        }
        // Chats that were never written to keep theirs, up to a few: a starting chief always does.
        if self.store.chief_choices.len() >= MAX_CHOICES {
            let starting: HashSet<String> = self.live_chiefs().map(|s| s.thread.clone()).collect();
            self.store.chief_choices.retain(|thread, _| starting.contains(thread));
        }
        self.store.chief_choices.insert(thread.to_owned(), choice.clone());
        self.store.save_later(&paths::store_file()).map_err(|error| error.to_string())
    }

    /// A chat of the chief's own for ctl and the chief's tools: `chat`, else a new one. `chief` picks the chief it
    /// starts with, `text` is its first message (or the next) and starts it; `front` brings it in front, else the
    /// thread in front, and a calling chief's own, stay as they are.
    pub(super) fn open_chat(
        &mut self,
        chat: Option<String>,
        chief: Option<&ChiefSelection>,
        text: Option<&str>,
        front: bool,
    ) -> Result<serde_json::Value, String> {
        let chat = match chat {
            Some(chat) => chat,
            None => format!("chat-{}", atomics_core::ticket_ui::request_id().map_err(|e| e.to_string())?),
        };
        if !atomics_core::trace::valid_chat_id(&chat) {
            return Err("Invalid chat session".into());
        }
        if let Some(choice) = chief {
            self.choose_thread_chief(&chat, choice)?;
        }
        if let Some(text) = text.map(str::trim).filter(|text| !text.is_empty()) {
            if text.len() > atomics_core::ops::MAX_TEXT_BYTES {
                return Err("The message is too long".into());
            }
            self.deliver_to_thread(&chat, &format!("[Chat {chat}]\n{text}"), false)?;
        }
        if front {
            self.open_thread(&chat, &ControlOrigin::Local)?;
        }
        let session = self.chief_of(&chat).map(|index| self.sessions[index].id.clone());
        Ok(serde_json::json!({ "chat": chat, "session": session }))
    }

    /// [`Self::open_thread`], with the chief it starts with where one is picked ([`Self::choose_thread_chief`]).
    pub(super) fn open_chief_chat(
        &mut self,
        thread: &str,
        chief: Option<&ChiefSelection>,
        origin: &ControlOrigin,
    ) -> Result<String, String> {
        if let Some(choice) = chief {
            self.choose_thread_chief(thread, choice)?;
        }
        self.open_thread(thread, origin)
    }

    /// Brings `thread` in front. Nothing is started and nothing ends: a chief that rests starts when it is written to.
    pub(super) fn open_thread(&mut self, thread: &str, origin: &ControlOrigin) -> Result<String, String> {
        if !thread.is_empty() && !atomics_core::trace::valid_chat_id(thread) {
            return Err("Invalid chat session".into());
        }
        if let Some(chief) = self.chief_of(thread) {
            self.check_input_access(self.sessions[chief].input_access, origin).map_err(|e| e.message)?;
        }
        if self.store.active_chief_chat != thread {
            let previous = std::mem::replace(&mut self.store.active_chief_chat, thread.to_owned());
            if let Err(error) = self.store.save_later(&paths::store_file()) {
                self.store.active_chief_chat = previous;
                return Err(error.to_string());
            }
        }
        Ok(self.chief().map(|s| s.id.clone()).unwrap_or_default())
    }

    /// Closes the chiefs that rest: out of view, with nothing open, pending or typed for a while. The workspace's own stays:
    /// it hears what is nobody's.
    pub(super) fn rest_chiefs(&mut self) {
        let resting: Vec<String> = self
            .live_chiefs()
            .filter(|s| !s.thread.is_empty() && s.thread != self.store.active_chief_chat)
            .filter(|s| s.hooks_active && s.claude_id.is_some())
            .filter(|s| matches!(s.status, AgentStatus::Idle | AgentStatus::Done) && !super::chief_provider::busy(s))
            .filter(|s| s.activity.wake_since.is_some_and(|at| at.elapsed() >= SLEEP_AFTER))
            .filter(|s| {
                s.terminal.as_ref().is_some_and(|t| t.last_typed().is_none_or(|at| at.elapsed() >= SLEEP_AFTER))
            })
            .filter(|s| self.chief_switch.as_ref().is_none_or(|switch| switch.old.as_deref() != Some(&s.id)))
            .filter(|s| !self.thread_works(&s.thread) && self.store.chief_threads.contains_key(&s.thread))
            .map(|s| s.id.clone())
            .collect();
        for id in resting {
            self.close_session(&id, false);
        }
    }

    /// Delivers `text` to the chief of `thread`, starting it with `text` as its first prompt where none runs.
    pub(super) fn deliver_to_thread(&mut self, thread: &str, text: &str, now: bool) -> Result<Delivery, String> {
        let chief = self.chief_of(thread).filter(|&index| self.sessions[index].current_chief());
        match chief {
            Some(index) => match self.deliver_with(index, text, now) {
                Delivery::Paused => Err("The host is preparing to stop".into()),
                Delivery::Unavailable if !super::chief::chief_takes_messages(&self.sessions[index]) => {
                    Err(super::chief::CLI_CHIEF_INPUT.into())
                }
                Delivery::Unavailable => Err("The chief cannot receive messages right now".into()),
                delivery => Ok(delivery),
            },
            None => {
                self.start_thread(thread, Some(text.to_string()), ControlOrigin::Local)?;
                Ok(Delivery::Sent)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{session, with_app};
    use super::*;
    use atomics_core::heartbeat::{Finding, Kind};

    const CHAT: &str = "chat-0123456789abcdef0123456789abcdef";

    fn chief(id: &str, thread: &str, dir: &Path) -> Session {
        let mut chief = session(id, AgentKind::Claude, AgentStatus::Idle, dir);
        chief.chief = true;
        chief.thread = thread.into();
        chief.claude_id = Some(format!("root-{id}"));
        chief.hooks_active = true;
        chief
    }

    fn board(id: &str, thread: &str, agent: &str) -> Plan {
        Plan {
            id: id.into(),
            thread: thread.into(),
            tasks: vec![Task {
                id: "t1".into(),
                session: Some(agent.into()),
                status: TaskStatus::Running,
                ..Task::default()
            }],
            ..Plan::default()
        }
    }

    fn idle_terminal(state: &State, dir: &Path) -> Terminal {
        let wake = state.wake.clone();
        Terminal::spawn(wake, Vec::new(), "/bin/cat".into(), Vec::new(), dir, HashMap::new(), 100).unwrap()
    }

    fn news(session: &str, plan: Option<&str>) -> StatusEvent {
        StatusEvent {
            seq: 1,
            session: session.into(),
            title: String::new(),
            status: AgentStatus::Done,
            at: 0,
            plan: plan.map(str::to_owned),
            task: None,
            report: None,
            approval: None,
            findings: None,
            ticket: None,
            config: None,
            change: None,
        }
    }

    #[test]
    fn a_boards_news_go_to_its_own_chief_and_to_the_lead_while_that_rests() {
        with_app(
            "app::threads::tests::a_boards_news_go_to_its_own_chief_and_to_the_lead_while_that_rests",
            |app, _| {
                let state = &mut *app.runtime;
                let dir = state.launch_dir.clone();
                state.sessions = vec![chief("workspace", "", &dir), chief("fresh", CHAT, &dir)];
                for agent in ["a", "b", "loose"] {
                    state.sessions.push(session(agent, AgentKind::Claude, AgentStatus::Working, &dir));
                }
                state.plans = vec![board("old", "", "a"), board("new", CHAT, "b")];
                let heard = |state: &State, event: &StatusEvent| (state.hears("", event), state.hears(CHAT, event));
                // By the plan an event names, or the one its session works for; what is nobody's is the lead's.
                assert_eq!(heard(state, &news("a", None)), (true, false));
                assert_eq!(heard(state, &news("b", None)), (false, true));
                assert_eq!(heard(state, &news("x", Some("new"))), (false, true));
                assert_eq!(heard(state, &news("loose", None)), (true, false));
                assert!(state.thread_works("") && state.thread_works(CHAT));
                let seen = |state: &State, thread: &str| {
                    let seen = state.sessions.iter().filter(|s| state.hears_session(thread, s));
                    seen.map(|s| s.id.clone()).collect::<Vec<_>>()
                };
                assert_eq!(seen(state, ""), ["workspace", "a", "loose"]);
                assert_eq!(seen(state, CHAT), ["fresh", "b"]);
                // That a chief ended its turn is no news to itself, nor to another.
                assert_eq!(heard(state, &news("fresh", None)), (false, false));
                assert_eq!(heard(state, &news("workspace", None)), (false, false));
                // A heartbeat goes to each chief with findings about its own boards, and to the lead.
                let about = |plan: &str| Finding {
                    plan: plan.into(),
                    task: "t1".into(),
                    session: None,
                    kind: Kind::Unreported,
                    message: None,
                    disk: None,
                };
                let beat = StatusEvent { findings: Some(vec![about("new")]), ..news("", None) };
                assert_eq!(heard(state, &beat), (true, true));
                assert_eq!(
                    (state.hears_finding("", &about("new")), state.hears_finding(CHAT, &about("new"))),
                    (false, true)
                );
                let quiet = StatusEvent { findings: Some(vec![]), ..news("", None) };
                assert_eq!(heard(state, &quiet), (true, false));
                // Without its chief, the lead hears of the board, so that nothing is lost.
                state.sessions.retain(|s| s.id != "fresh");
                assert!(state.hears("", &news("b", None)));
                // Without the workspace's, the one in front leads.
                state.sessions = vec![chief("fresh", CHAT, &dir)];
                state.store.active_chief_chat = CHAT.into();
                assert!(state.hears(CHAT, &news("a", None)));
            },
        );
    }

    #[test]
    fn a_chief_that_waits_for_the_first_time_is_told_what_came_since_it_started() {
        with_app(
            "app::threads::tests::a_chief_that_waits_for_the_first_time_is_told_what_came_since_it_started",
            |app, _| {
                let state = &mut *app.runtime;
                let dir = state.launch_dir.clone();
                state.sessions = vec![chief("workspace", "", &dir), chief("fresh", CHAT, &dir)];
                state.sessions.push(session("b", AgentKind::Claude, AgentStatus::Working, &dir));
                state.plans = vec![board("new", CHAT, "b")];
                state.record_threads();
                let before = state.event_seq;
                // Its agent was done before it waited, as after an approval that woke it.
                state.event_seq += 1;
                state.events.push(StatusEvent { seq: before + 1, ..news("b", None) });
                let wait = |state: &mut State, sessions: &[&str]| {
                    let (reply, answer) = std::sync::mpsc::channel();
                    state.control_caller = Some("fresh".into());
                    let request = ControlRequest::Wait {
                        sessions: sessions.iter().map(|s| s.to_string()).collect(),
                        statuses: vec![],
                        after: None,
                        timeout_secs: Some(30),
                        compact: true,
                        full_sessions: false,
                    };
                    state.control(request, reply);
                    state.control_caller = None;
                    match answer.try_recv() {
                        Ok(ControlOutcome::Done(response)) => {
                            let events = response.result["events"].as_array().cloned().unwrap();
                            events.iter().map(|e| e["session"].as_str().unwrap().to_owned()).collect()
                        }
                        _ => vec!["(waits)".to_string()],
                    }
                };
                assert_eq!(wait(state, &["b"]), ["(waits)"], "what a named session does next");
                assert_eq!(wait(state, &[]), ["b"]);
                assert_eq!(wait(state, &[]), ["(waits)"], "it was told");
            },
        );
    }

    #[test]
    fn a_thread_continues_the_conversation_it_was_last_in_also_after_a_clear() {
        with_app(
            "app::threads::tests::a_thread_continues_the_conversation_it_was_last_in_also_after_a_clear",
            |app, _| {
                let state = &mut *app.runtime;
                let dir = state.launch_dir.clone();
                state.config.chief.resume = Some("pinned".into());
                let preferred = state.preferred_chief();
                assert_eq!(state.thread_start(""), (preferred.clone(), Some("pinned".into())), "the first start");
                assert_eq!(state.thread_start(CHAT), (preferred.clone(), None), "a fresh thread begins anew");
                state.sessions = vec![chief("workspace", "", &dir), chief("fresh", CHAT, &dir)];
                state.record_threads();
                state.sessions[0].claude_id = Some("after-clear".into());
                state.record_threads();
                assert_eq!(state.thread_start("").1.as_deref(), Some("after-clear"), "not the pinned one again");
                assert_eq!(state.thread_start(CHAT).1.as_deref(), Some("root-fresh"));
                // What an older version saved comes back to the thread its conversation is of.
                let saved = |conversation: &str, thread: &str| {
                    let session = chief("saved", thread, &dir);
                    state.session_record_titled(&session, conversation.into(), None, String::new())
                };
                assert_eq!(state.thread_of_record(&saved("root-fresh", "")), CHAT);
                assert_eq!(state.thread_of_record(&saved("after-clear", "")), "");
                assert_eq!(state.thread_of_record(&saved("", "")), "");
                assert_eq!(state.thread_of_record(&saved("after-clear", CHAT)), CHAT);
                // Another pin is a wish of the user's, and is followed once.
                state.config.chief.resume = Some("another".into());
                assert_eq!(state.thread_start("").1.as_deref(), Some("another"));
                state.sessions[0].claude_id = Some("another".into());
                state.record_threads();
                state.sessions[0].claude_id = Some("cleared-again".into());
                state.record_threads();
                assert_eq!(state.thread_start("").1.as_deref(), Some("cleared-again"));
                // A conversation that cannot be continued ends before its hooks report; the next start is a fresh one.
                state.sessions[1].hooks_active = false;
                state.forget_unstarted(1);
                assert_eq!(state.thread_start(CHAT), (preferred, None));
            },
        );
    }

    #[test]
    fn a_chief_rests_out_of_view_with_nothing_open() {
        with_app("app::threads::tests::a_chief_rests_out_of_view_with_nothing_open", |app, _| {
            let state = &mut *app.runtime;
            let dir = state.launch_dir.clone();
            let before = Instant::now().checked_sub(SLEEP_AFTER + Duration::from_secs(1)).unwrap();
            let mut resting = chief("fresh", CHAT, &dir);
            resting.activity.wake_since = Some(before);
            resting.terminal = Some(idle_terminal(state, &dir));
            state.sessions = vec![chief("workspace", "", &dir), resting];
            state.record_threads();
            let alive = |state: &State| state.chief_of(CHAT).is_some();
            state.plans = vec![board("new", CHAT, "b")];
            state.rest_chiefs();
            assert!(alive(state), "its board works");
            state.plans[0].tasks[0].status = TaskStatus::Done;
            state.store.active_chief_chat = CHAT.into();
            state.rest_chiefs();
            assert!(alive(state), "it is in front");
            state.store.active_chief_chat = String::new();
            state.rest_chiefs();
            assert!(!alive(state));
            assert_eq!(state.thread_start(CHAT).1.as_deref(), Some("root-fresh"), "and continues when written to");
            // The workspace's own stays, out of view too.
            let mut own = chief("workspace", "", &dir);
            own.activity.wake_since = Some(before);
            own.terminal = Some(idle_terminal(state, &dir));
            state.sessions = vec![own];
            state.store.active_chief_chat = CHAT.into();
            state.rest_chiefs();
            assert!(state.chief_of("").is_some());
        });
    }
}
