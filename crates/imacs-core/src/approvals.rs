use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::hook::HookEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    AllowAlways,
    Deny,
}

impl Decision {
    pub fn output(self, message: Option<&str>) -> String {
        let mut decision = json!({"behavior": if self == Self::Deny { "deny" } else { "allow" }});
        if let Some(message) = message {
            decision["message"] = json!(message);
        }
        json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": decision}}).to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub tool: String,
    pub command: Option<String>,
    pub cwd: PathBuf,
    pub reason: Option<String>,
    pub input: Value,
}

impl ApprovalRequest {
    pub fn from_event(event: &HookEvent) -> Self {
        let text = |key: &str| event.tool_input.get(key).and_then(Value::as_str).map(str::to_string);
        Self {
            tool: event.tool_name.clone().unwrap_or_default(),
            command: text("command"),
            cwd: text("cwd")
                .or_else(|| text("workdir"))
                .map(PathBuf::from)
                .or_else(|| event.cwd.clone())
                .unwrap_or_default(),
            reason: text("description").or_else(|| text("justification")),
            input: event.tool_input.clone(),
        }
    }

    pub fn same_command(&self, other: &Self) -> bool {
        self.tool == other.tool
            && self.cwd == other.cwd
            && self.command == other.command
            && (self.command.is_some() || self.input == other.input)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApprovalPolicy {
    pub allow: Vec<String>,
}

impl Default for ApprovalPolicy {
    fn default() -> Self {
        Self {
            allow: ["ls", "cat", "rg", "git status", "git diff", "git log"].into_iter().map(str::to_string).collect(),
        }
    }
}

impl ApprovalPolicy {
    pub fn allows(&self, request: &ApprovalRequest, root: &Path) -> bool {
        if request.tool != "Bash" || !inside(&request.cwd, root) {
            return false;
        }
        let Some(command) = &request.command else { return false };
        // Refuse shell expansion, compound commands and redirections, even when quoted.
        if command.contains([
            '\n', '\r', ';', '|', '&', '<', '>', '`', '$', '*', '?', '[', ']', '{', '}', '~', '(', ')', '\\',
        ]) {
            return false;
        }
        let Some(words) = shlex::split(command) else { return false };
        if words.is_empty()
            || !self.allow.iter().any(|prefix| {
                shlex::split(prefix).is_some_and(|prefix| !prefix.is_empty() && words.starts_with(&prefix))
            })
        {
            return false;
        }
        for word in &words {
            // These read-tool flags can execute programs or write files.
            if [
                "--pre",
                "--hostname-bin",
                "--ext-diff",
                "--textconv",
                "--output",
                "--config",
                "--config-path",
                "--exec-path",
                "--git-dir",
                "--work-tree",
                "--directory",
                "--out-dir",
                "--target-dir",
                "--follow",
                "--dereference",
                "--dereference-command-line",
                "--dereference-command-line-symlink-to-dir",
            ]
            .iter()
            .any(|flag| word == flag || word.starts_with(&format!("{flag}=")))
                || ["-c", "-C", "-o", "-O", "-f"]
                    .iter()
                    .any(|flag| !word.starts_with("--") && word.starts_with(flag) && word.len() > flag.len())
                || matches!(word.as_str(), "-c" | "-C" | "-o")
                || (words[0] == "ls" && word.starts_with('-') && !word.starts_with("--") && word.contains('L'))
                || (words[0] == "rg" && word.starts_with('-') && !word.starts_with("--") && word.contains('L'))
            {
                return false;
            }
            let value = word.split_once('=').map_or(word.as_str(), |(_, value)| value);
            if (value.contains('/') || value == "..") && !inside(&request.cwd.join(value), root) {
                return false;
            }
            if !word.starts_with('-') && request.cwd.join(word).exists() && !inside(&request.cwd.join(word), root) {
                return false;
            }
        }
        true
    }
}

fn inside(path: &Path, root: &Path) -> bool {
    fn resolve(path: &Path) -> Option<PathBuf> {
        if !path.is_absolute() {
            return None;
        }
        let mut resolved = PathBuf::new();
        for component in path.components() {
            match component {
                Component::ParentDir => {
                    resolved.pop();
                }
                Component::CurDir => {}
                other => resolved.push(other.as_os_str()),
            }
            if resolved.exists() {
                resolved = resolved.canonicalize().ok()?;
            }
        }
        Some(resolved)
    }
    matches!((resolve(path), resolve(root)), (Some(path), Some(root)) if path.starts_with(&root))
}

/// Codex also prompts for edits and terminal input; their choices must never receive steering text.
pub fn menu_visible(screen: &str) -> bool {
    let compact: String = screen.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect();
    compact.contains("pressentertoconfirmoresctocancel")
        || screen.lines().any(|line| {
            let line = line.trim();
            let Some(choice) = line.strip_prefix(['›', '❯', '>']) else { return false };
            let Some((number, _)) = choice.trim().split_once('.') else { return false };
            !number.is_empty() && number.chars().all(|c| c.is_ascii_digit())
        })
}

/// Only answer a native menu whose numbered choices are actually visible.
pub fn menu_key(screen: &str, command: Option<&str>, decision: Decision) -> Option<String> {
    let command = command?;
    let (_, menu) = screen.rsplit_once("Would you like to run the following command?")?;
    let (_, shown) = menu.split_once('$')?;
    let shown = shown.split("\n\n").next()?;
    let compact = |text: &str| text.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    if compact(shown) != compact(command) {
        return None;
    }
    if decision == Decision::Deny && menu.contains("esc to cancel") {
        return Some("\x1b".into());
    }
    let needle = match decision {
        Decision::Allow | Decision::AllowAlways => "yes, proceed",
        Decision::Deny => "no, and tell codex",
    };
    menu.lines().find_map(|line| {
        let line = line.trim().trim_start_matches(['›', '❯', ' ']);
        let (number, label) = line.split_once('.')?;
        (number.len() == 1
            && number.chars().all(|c| c.is_ascii_digit())
            && label.trim().to_lowercase().starts_with(needle))
        .then(|| format!("{number}\r"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(command: &str, cwd: &Path) -> ApprovalRequest {
        ApprovalRequest {
            tool: "Bash".into(),
            command: Some(command.into()),
            cwd: cwd.into(),
            reason: None,
            input: Value::Null,
        }
    }

    #[test]
    fn default_policy_allows_only_worktree_reads() {
        let dir = tempfile::tempdir().unwrap();
        for command in ["ls -la", "cat Cargo.toml", "rg hook src", "git status --short", "git diff", "git log -5"] {
            assert!(ApprovalPolicy::default().allows(&request(command, dir.path()), dir.path()), "{command}");
        }
    }

    #[test]
    fn cargo_requires_an_explicit_policy_entry() {
        let dir = tempfile::tempdir().unwrap();
        for command in ["cargo build", "cargo test", "cargo check", "cargo clippy", "cargo fmt"] {
            assert!(!ApprovalPolicy::default().allows(&request(command, dir.path()), dir.path()));
        }
        assert!(
            ApprovalPolicy { allow: vec!["cargo test".into()] }
                .allows(&request("cargo test -p imacs-core", dir.path()), dir.path())
        );
    }

    #[test]
    fn policy_waits_for_writes_shell_syntax_external_paths_and_execution_flags() {
        let dir = tempfile::tempdir().unwrap();
        for command in [
            "cargo publish",
            "rm file",
            "git push",
            "git status; touch pwned",
            "cat $(pwd)",
            "ls | sh",
            "cat a > b",
            "rg --pre=sh x",
            "git diff --output=file",
            "git diff --ext-diff",
            "git log --textconv",
            "cat /etc/passwd",
            "cargo test --manifest-path=../Cargo.toml",
            "cat ../secret",
            "cargo test --target-dir=/tmp/elsewhere",
            "rg -f/etc/passwd pattern",
            "git diff -O/etc/passwd",
            "rg --follow pattern",
            "ls -RL",
        ] {
            assert!(!ApprovalPolicy::default().allows(&request(command, dir.path()), dir.path()), "{command}");
        }
        assert!(!ApprovalPolicy::default().allows(&request("ls", Path::new("/")), dir.path()));
        assert!(!ApprovalPolicy { allow: vec![] }.allows(&request("ls", dir.path()), dir.path()));
        assert!(
            !ApprovalPolicy::default()
                .allows(&ApprovalRequest { tool: "mcp__x__y".into(), ..request("ls", dir.path()) }, dir.path())
        );
    }

    #[test]
    fn policy_resolves_symlinks_and_custom_prefixes() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        for command in ["cat escape/secret", "ls escape", "cat escape/../secret"] {
            assert!(!ApprovalPolicy::default().allows(&request(command, root.path()), root.path()));
        }
        let policy = ApprovalPolicy { allow: vec!["echo".into()] };
        assert!(policy.allows(&request("echo hello", root.path()), root.path()));
        assert!(!policy.allows(&request("echolocation hello", root.path()), root.path()));
    }

    #[test]
    fn permission_payload_and_response_match_codex_contract() {
        let event = HookEvent::from_payload(&json!({"session_id":"s", "hook_event_name":"PermissionRequest", "cwd":"/repo", "tool_name":"Bash", "tool_input":{"command":"touch hello", "cwd":"/repo/sub", "description":"Create a test file"}})).unwrap();
        let request = ApprovalRequest::from_event(&event);
        assert_eq!(event.status(), Some(crate::session::AgentStatus::NeedsInput));
        assert_eq!(request.command.as_deref(), Some("touch hello"));
        assert_eq!(request.cwd, Path::new("/repo/sub"));
        assert_eq!(request.reason.as_deref(), Some("Create a test file"));
        for decision in [Decision::Allow, Decision::AllowAlways, Decision::Deny] {
            let output: Value = serde_json::from_str(&decision.output(Some("because"))).unwrap();
            assert_eq!(
                output["hookSpecificOutput"]["decision"]["behavior"],
                if decision == Decision::Deny { "deny" } else { "allow" }
            );
            assert_eq!(output["hookSpecificOutput"]["decision"]["message"], "because");
            assert!(output["hookSpecificOutput"]["decision"].get("updatedPermissions").is_none());
        }
    }

    #[test]
    fn recognizes_other_codex_menus_without_a_command_prompt() {
        for screen in [
            "Allow these edits?\n› 1. Yes, proceed\n  2. No",
            "Terminal input required\nPress enter to confirm or esc to cancel",
            "Permissions changed\nPress enter to confirm or\nesc to cancel",
            "Different display\n❯ 2. Allow once",
        ] {
            assert!(menu_visible(screen), "{screen}");
            assert!(menu_key(screen, Some("touch fixture"), Decision::Allow).is_none());
        }
        assert!(!menu_visible("1. A normal numbered response\n› Ask Codex to do anything"));
    }

    #[test]
    fn fallback_only_answers_visible_matching_menu_choices() {
        let screen = "Would you like to run the following command?\n\n  $ touch fixture\n\n› 1. Yes, proceed (y)\n  2. Yes, and don't ask again for commands that start with `touch`\n  3. No, and tell Codex what to do differently (esc)";
        assert_eq!(menu_key(screen, Some("touch fixture"), Decision::Allow).as_deref(), Some("1\r"));
        assert_eq!(menu_key(screen, Some("touch fixture"), Decision::AllowAlways).as_deref(), Some("1\r"));
        assert_eq!(menu_key(screen, Some("touch fixture"), Decision::Deny).as_deref(), Some("3\r"));
        assert_eq!(menu_key("Agent is working", Some("touch fixture"), Decision::Allow), None);
        assert_eq!(menu_key(screen, Some("touch other"), Decision::Allow), None);
        let old = format!("9. Yes, proceed (y)\n{screen}");
        assert_eq!(menu_key(&old, Some("touch fixture"), Decision::Allow).as_deref(), Some("1\r"));
        let no_choice =
            "1. Yes, proceed (y)\nWould you like to run the following command?\n\n$ touch fixture\n\nRendering menu";
        assert_eq!(menu_key(no_choice, Some("touch fixture"), Decision::Allow), None);
        let clipped = "Would you like to run the following command?\n\n  $ touch fixture\n\n› 1. Yes, proceed (y)\nPress enter to confirm or esc to cancel";
        assert_eq!(menu_key(clipped, Some("touch fixture"), Decision::Deny).as_deref(), Some("\x1b"));
    }
}
