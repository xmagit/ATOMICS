//! Declarative lifecycle after acceptance. Hosts report stages; the core never polls CI.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Workflow {
    pub done: String,
    /// Successful stages in order. ci_failed is a side event, never a completion stage.
    pub stages: Vec<String>,
}

impl Default for Workflow {
    fn default() -> Self {
        Self {
            done: "accepted".into(),
            stages: ["accepted", "integrated", "pr_opened", "ci_passed", "merged", "deployed"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stage {
    pub stage: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Stage {
    pub fn label(&self) -> String {
        match self.stage.as_str() {
            "pr_opened" => "PR opened".into(),
            "ci_passed" => "CI passed".into(),
            "ci_failed" => "CI failed".into(),
            name => {
                let mut label = name.replace('_', " ");
                if let Some(first) = label.get_mut(..1) {
                    first.make_ascii_uppercase();
                }
                label
            }
        }
    }

    pub fn comment(&self) -> String {
        let mut text = self.label();
        if let Some(url) = &self.url {
            text.push_str(&format!(": {url}"));
        }
        if let Some(note) = &self.note {
            text.push_str(&format!("\n\n{note}"));
        }
        text
    }
}

impl Workflow {
    pub fn validate(&self) -> Result<(), String> {
        if self.stages.first().map(String::as_str) != Some("accepted") {
            return Err("workflow.stages must start with accepted".into());
        }
        for (index, stage) in self.stages.iter().enumerate() {
            if stage.is_empty()
                || !stage.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                || stage == "ci_failed"
                || self.stages[..index].contains(stage)
            {
                return Err(format!("invalid or duplicate workflow stage {stage:?}"));
            }
        }
        if !self.stages.contains(&self.done) {
            return Err(format!("unknown workflow.done stage {:?}", self.done));
        }
        Ok(())
    }

    pub fn allows(&self, stage: &str) -> bool {
        stage == "ci_failed" || self.stages.iter().any(|s| s == stage)
    }

    /// A later successful stage proves earlier milestones were reached. Failure never does.
    pub fn reached(&self, history: &[Stage]) -> bool {
        let Some(gate) = self.stages.iter().position(|s| s == &self.done) else { return false };
        gate == 0
            || history
                .iter()
                .any(|event| self.stages.iter().position(|s| s == &event.stage).is_some_and(|rank| rank >= gate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_stages_gate_completion_and_failure_is_not_success() {
        let mut workflow = Workflow { done: "ci_passed".into(), ..Default::default() };
        let event = |name: &str| Stage { stage: name.into(), url: None, note: None };
        assert!(workflow.validate().is_ok());
        assert!(!workflow.reached(&[event("pr_opened"), event("ci_failed")]));
        assert!(workflow.reached(&[event("ci_passed")]));
        assert!(workflow.reached(&[event("merged")]));
        workflow.stages.insert(4, "security_approved".into());
        workflow.done = "security_approved".into();
        assert!(workflow.validate().is_ok());
        assert!(!workflow.reached(&[event("ci_passed")]));
        assert!(workflow.reached(&[event("security_approved")]));
        workflow.done = "ci_failed".into();
        assert!(workflow.validate().is_err());
    }
}
