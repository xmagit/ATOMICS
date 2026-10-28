//! How much heavy work the machine runs at once. Compilers of every agent share one
//! jobserver pool; whole test runs and QA instances wait for a slot. The app owns the
//! pool and the waiting; this is the policy and the queue itself.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotClass {
    Test,
    Qa,
}

impl SlotClass {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "test" => Some(Self::Test),
            "qa" => Some(Self::Qa),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Qa => "qa",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceConfig {
    /// Tokens in the jobserver shared by every agent's compilers; unset = cores minus 2.
    pub jobs: Option<usize>,
    /// Whole test runs at once (`imacs queue --class test`); unset = one per 6 cores.
    pub test_slots: Option<usize>,
    /// QA instances at once (`imacs queue --class qa`); unset = 1.
    pub qa_slots: Option<usize>,
    /// Niceness agent terminals start at, so IMACS and the desktop stay responsive.
    pub agent_nice: i32,
}

impl Default for ResourceConfig {
    fn default() -> Self {
        Self { jobs: None, test_slots: None, qa_slots: None, agent_nice: 10 }
    }
}

impl ResourceConfig {
    pub fn jobs(&self, cores: usize) -> usize {
        self.jobs.unwrap_or(cores.saturating_sub(2)).max(1)
    }

    pub fn slots(&self, class: SlotClass, cores: usize) -> usize {
        match class {
            SlotClass::Test => self.test_slots.unwrap_or(cores / 6),
            SlotClass::Qa => self.qa_slots.unwrap_or(1),
        }
        .max(1)
    }
}

/// One request for a slot: what it runs and the processes it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Ticket {
    pub id: u64,
    pub class: SlotClass,
    pub label: String,
    pub ancestors: Vec<u32>,
}

/// First come, first served within each class.
#[derive(Debug, Default)]
pub struct SlotQueue {
    running: Vec<Ticket>,
    waiting: VecDeque<Ticket>,
    next_id: u64,
}

impl SlotQueue {
    pub fn enqueue(&mut self, class: SlotClass, label: String, ancestors: Vec<u32>) -> u64 {
        self.next_id += 1;
        self.waiting.push_back(Ticket { id: self.next_id, class, label, ancestors });
        self.next_id
    }

    /// Moves the oldest waiting tickets of each class into its free slots.
    pub fn grant(&mut self, limit: impl Fn(SlotClass) -> usize) {
        let mut index = 0;
        while index < self.waiting.len() {
            let class = self.waiting[index].class;
            if self.running.iter().filter(|t| t.class == class).count() < limit(class) {
                let ticket = self.waiting.remove(index).expect("index in range");
                self.running.push(ticket);
            } else {
                index += 1;
            }
        }
    }

    pub fn is_running(&self, id: u64) -> bool {
        self.running.iter().any(|t| t.id == id)
    }

    /// 1-based place among the waiting tickets of its class.
    pub fn position(&self, id: u64) -> Option<usize> {
        let class = self.waiting.iter().find(|t| t.id == id)?.class;
        self.waiting.iter().filter(|t| t.class == class).position(|t| t.id == id).map(|p| p + 1)
    }

    pub fn release(&mut self, id: u64) {
        self.running.retain(|t| t.id != id);
        self.waiting.retain(|t| t.id != id);
    }

    pub fn running(&self) -> &[Ticket] {
        &self.running
    }

    pub fn waiting(&self) -> impl Iterator<Item = &Ticket> {
        self.waiting.iter()
    }
}

/// The command inside `imacs queue [--class <c>] -- <command>`, so policies judge what
/// actually runs. Only a bare `imacs` counts: IMACS puts its own binary first on agents' PATH,
/// while a path could name any program called imacs.
pub fn queued_command(words: &[String]) -> Option<&[String]> {
    if words.first().map(String::as_str) != Some("imacs") || words.get(1).map(String::as_str) != Some("queue") {
        return None;
    }
    let separator = words.iter().position(|w| w == "--")?;
    let valid = match &words[2..separator] {
        [] => true,
        [option] => option.strip_prefix("--class=").and_then(SlotClass::parse).is_some(),
        [flag, class] => flag == "--class" && SlotClass::parse(class).is_some(),
        _ => false,
    };
    (valid && separator + 1 < words.len()).then(|| &words[separator + 1..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(test: usize, qa: usize) -> impl Fn(SlotClass) -> usize {
        move |class| match class {
            SlotClass::Test => test,
            SlotClass::Qa => qa,
        }
    }

    #[test]
    fn defaults_follow_the_core_count() {
        let config = ResourceConfig::default();
        assert_eq!(config.jobs(18), 16);
        assert_eq!(config.jobs(1), 1);
        assert_eq!(config.slots(SlotClass::Test, 18), 3);
        assert_eq!(config.slots(SlotClass::Test, 4), 1);
        assert_eq!(config.slots(SlotClass::Qa, 18), 1);
        let fixed = ResourceConfig { jobs: Some(6), test_slots: Some(0), qa_slots: Some(2), agent_nice: 0 };
        assert_eq!((fixed.jobs(18), fixed.slots(SlotClass::Test, 18), fixed.slots(SlotClass::Qa, 18)), (6, 1, 2));
    }

    #[test]
    fn slots_are_granted_oldest_first_per_class() {
        let mut queue = SlotQueue::default();
        let a = queue.enqueue(SlotClass::Test, "a".into(), vec![]);
        let b = queue.enqueue(SlotClass::Test, "b".into(), vec![]);
        let q = queue.enqueue(SlotClass::Qa, "q".into(), vec![]);
        let c = queue.enqueue(SlotClass::Test, "c".into(), vec![]);
        queue.grant(limits(1, 1));
        assert!(queue.is_running(a) && queue.is_running(q));
        assert!(!queue.is_running(b) && !queue.is_running(c));
        assert_eq!((queue.position(b), queue.position(c), queue.position(a)), (Some(1), Some(2), None));
        queue.release(a);
        queue.grant(limits(1, 1));
        assert!(queue.is_running(b) && !queue.is_running(c));
        assert_eq!(queue.position(c), Some(1));
    }

    #[test]
    fn a_waiter_that_leaves_frees_its_place() {
        let mut queue = SlotQueue::default();
        let a = queue.enqueue(SlotClass::Test, "a".into(), vec![]);
        let b = queue.enqueue(SlotClass::Test, "b".into(), vec![]);
        let c = queue.enqueue(SlotClass::Test, "c".into(), vec![]);
        queue.grant(limits(1, 1));
        queue.release(b);
        assert_eq!(queue.position(c), Some(1));
        queue.release(a);
        queue.grant(limits(1, 1));
        assert!(queue.is_running(c));
        assert_eq!(queue.waiting().count(), 0);
    }

    #[test]
    fn queued_command_is_the_part_after_the_separator() {
        let words = |text: &str| text.split(' ').map(str::to_string).collect::<Vec<_>>();
        let inner = |text: &str| queued_command(&words(text)).map(|w| w.join(" "));
        assert_eq!(inner("imacs queue -- cargo test"), Some("cargo test".into()));
        assert_eq!(inner("imacs queue --class test -- cargo test -p x"), Some("cargo test -p x".into()));
        assert_eq!(inner("/Apps/IMACS.app/Contents/MacOS/imacs queue -- cargo test"), None);
        assert_eq!(inner("./imacs queue -- cargo test"), None);
        assert_eq!(inner("imacs queue --class=qa -- python3 qa.py"), Some("python3 qa.py".into()));
        assert_eq!(inner("imacs queue --class nope -- cargo test"), None);
        assert_eq!(inner("imacs queue --class=nope -- cargo test"), None);
        assert_eq!(inner("imacs queue --"), None);
        assert_eq!(inner("imacs ctl -- cargo test"), None);
        assert_eq!(inner("notimacs queue -- cargo test"), None);
        assert_eq!(inner("cargo test"), None);
    }
}
