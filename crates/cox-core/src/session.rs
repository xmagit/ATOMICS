use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentStatus {
    Starting,
    /// Waiting for a prompt; nothing new to look at.
    Idle,
    Working,
    /// Blocked on a permission prompt.
    NeedsInput,
    /// Finished a turn you have not looked at yet.
    Done,
    Exited,
}

impl AgentStatus {
    pub fn needs_attention(self) -> bool {
        matches!(self, AgentStatus::NeedsInput | AgentStatus::Done)
    }

    pub fn label(self) -> &'static str {
        match self {
            AgentStatus::Starting => "starting",
            AgentStatus::Idle => "idle",
            AgentStatus::Working => "working",
            AgentStatus::NeedsInput => "needs you",
            AgentStatus::Done => "turn done",
            AgentStatus::Exited => "exited",
        }
    }

    /// Sort key: what needs you first, then what is running.
    pub fn urgency(self) -> u8 {
        match self {
            AgentStatus::NeedsInput => 0,
            AgentStatus::Done => 1,
            AgentStatus::Working => 2,
            AgentStatus::Starting => 3,
            AgentStatus::Idle => 4,
            AgentStatus::Exited => 5,
        }
    }
}

const BRANCH_PREFIXES: &[&str] = &["issue/", "feature/", "feat/", "fix/", "bugfix/", "chore/", "claude/"];

/// A short title from the branch, falling back to the folder name.
pub fn title_for(branch: Option<&str>, dir: &Path) -> String {
    if let Some(branch) = branch.filter(|b| !matches!(*b, "main" | "master")) {
        let trimmed = BRANCH_PREFIXES.iter().find_map(|p| branch.strip_prefix(p)).unwrap_or(branch);
        return trimmed.to_string();
    }
    dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| dir.display().to_string())
}

/// Picks which sessions get tiles: the active one always, then by urgency, keeping
/// the original order otherwise so tiles do not jump around.
pub fn pick_tiled(statuses: &[AgentStatus], active: Option<usize>, slots: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..statuses.len()).collect();
    order.sort_by_key(|&i| (Some(i) != active, statuses[i].urgency(), i));
    let mut picked: Vec<usize> = order.into_iter().take(slots).collect();
    picked.sort();
    picked
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn titles_strip_branch_prefixes() {
        let dir = PathBuf::from("/r/shop-42");
        assert_eq!(title_for(Some("issue/42-faster-checkout"), &dir), "42-faster-checkout");
        assert_eq!(title_for(Some("main"), &dir), "shop-42");
        assert_eq!(title_for(None, &dir), "shop-42");
    }

    #[test]
    fn active_and_urgent_sessions_win_tiles() {
        use AgentStatus::*;
        let statuses = [Idle, Working, Done, Idle, NeedsInput];
        assert_eq!(pick_tiled(&statuses, Some(3), 3), vec![2, 3, 4]);
        assert_eq!(pick_tiled(&statuses, None, 10), vec![0, 1, 2, 3, 4]);
    }
}
