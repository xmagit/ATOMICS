#[cfg(feature = "native")]
use std::path::{Component, Path};
use std::{borrow::Cow, path::PathBuf};

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
    /// What the host adds for whoever decides, such as whose shared stash entries the command drops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

impl ApprovalRequest {
    /// A decision names both the unique request and all the arguments shown for it.
    pub fn digest(&self, id: &str) -> String {
        use sha2::{Digest, Sha256};
        let bytes = serde_json::to_vec(&(id, self)).expect("approval request is JSON");
        format!("{:x}", Sha256::digest(bytes))
    }

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
            warning: None,
        }
    }

    /// The shared stash entries this request's command drops, by the entry they name.
    pub fn stash_drops(&self) -> Vec<crate::stash::StashDrop> {
        match &self.command {
            Some(command) if self.tool == "Bash" => crate::stash::drops(command),
            _ => Vec::new(),
        }
    }

    /// Whether "Allow always" may answer it. Never for an Azure request, nor for a command that drops
    /// shared stash entries: the same `stash@{n}` names another entry once anyone stashes.
    pub fn may_allow_always(&self) -> bool {
        self.tool != "Azure REST" && self.stash_drops().is_empty()
    }

    /// A remembered decision covers the same tool arguments. The provider may reword its explanation.
    pub fn same_permission(&self, other: &Self) -> bool {
        let arguments = |request: &Self| {
            let mut input = request.input.clone();
            if request.tool == "Bash"
                && let Some(fields) = input.as_object_mut()
            {
                for name in ["description", "justification"] {
                    if fields.get(name).and_then(Value::as_str).is_some_and(|s| Some(s) == request.reason.as_deref()) {
                        fields.remove(name);
                    }
                }
            }
            input
        };
        self.same_command(other) && arguments(self) == arguments(other)
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
    /// Exact NAME=value assignments, including an empty value after `=`.
    pub env: Vec<String>,
}

impl Default for ApprovalPolicy {
    fn default() -> Self {
        Self {
            allow: ["ls", "cat", "rg", "git status", "git diff", "git log"].into_iter().map(str::to_string).collect(),
            env: Vec::new(),
        }
    }
}

#[cfg(feature = "native")]
impl ApprovalPolicy {
    pub fn allows(&self, request: &ApprovalRequest, root: &Path) -> bool {
        if request.tool != "Bash" || !inside(&request.cwd, root) {
            return false;
        }
        let Some(command) = &request.command else { return false };
        // Whatever the allow list says, dropping shared stash entries waits for the chief or the user.
        if !request.stash_drops().is_empty() {
            return false;
        }
        let Some(commands) = shell_commands(command) else { return false };
        commands.iter().all(|words| self.allows_command(words, &request.cwd, root))
    }

    fn allows_command(&self, raw: &[&str], cwd: &Path, root: &Path) -> bool {
        let Some(words): Option<Vec<String>> = raw.iter().map(|word| shlex::split(word)?.pop()).collect() else {
            return false;
        };
        // Only an unquoted NAME= prefix is a shell assignment. `env` accepts quoted ones too.
        let assignments = raw.iter().take_while(|word| assignment(word)).count();
        if !words[..assignments].iter().all(|word| self.env.contains(word)) {
            return false;
        }
        let mut words = &words[assignments..];
        loop {
            if words.first().map(String::as_str) == Some("env") {
                words = &words[1..];
                let assignments = words.iter().take_while(|word| assignment(word)).count();
                if assignments == 0 || !words[..assignments].iter().all(|word| self.env.contains(word)) {
                    return false;
                }
                words = &words[assignments..];
            } else if let Some(inner) = crate::governor::queued_command(words) {
                // Queue executes argv directly: a bare assignment here is not a shell prefix.
                words = inner;
            } else {
                break;
            }
        }
        let Some(words) = self.cargo_command(words) else { return false };
        if words.is_empty()
            || !self.allow.iter().any(|prefix| {
                shlex::split(prefix)
                    .and_then(|prefix| self.cargo_command(&prefix))
                    .is_some_and(|prefix| !prefix.is_empty() && words.starts_with(&prefix))
            })
        {
            return false;
        }
        for word in &words[1..] {
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
                "--manifest-path",
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
                || (words[0] == "cargo"
                    && word.starts_with('-')
                    && !word.starts_with("--")
                    && word.contains(['Z', 'C']))
                || (words[0] == "ls" && word.starts_with('-') && !word.starts_with("--") && word.contains('L'))
                || (words[0] == "rg" && word.starts_with('-') && !word.starts_with("--") && word.contains('L'))
            {
                return false;
            }
            let value = word.split_once('=').map_or(word.as_str(), |(_, value)| value);
            if (value.contains('/') || value == "..") && !inside(&cwd.join(value), root) {
                return false;
            }
            if !word.starts_with('-') && cwd.join(word).exists() && !inside(&cwd.join(word), root) {
                return false;
            }
        }
        // Non-cargo programs still have to stay within the worktree themselves.
        if words[0].contains('/') && !inside(&cwd.join(&words[0]), root) {
            return false;
        }
        true
    }

    fn cargo_command(&self, words: &[String]) -> Option<Vec<String>> {
        let mut words = words;
        if words.first().map(String::as_str) == Some("rustup") {
            if words.get(1).map(String::as_str) != Some("run") || !toolchain(words.get(2)?) {
                return None;
            }
            words = &words[3..];
            if !self.is_cargo(words.first()?) {
                return None;
            }
        }
        let mut normalized = words.to_vec();
        if let Some(program) = words.first()
            && self.is_cargo(program)
        {
            normalized[0] = "cargo".into();
            if let Some(version) = words.get(1).and_then(|word| word.strip_prefix('+')) {
                if !toolchain(version) {
                    return None;
                }
                normalized.remove(1);
            }
        }
        Some(normalized)
    }

    fn is_cargo(&self, program: &str) -> bool {
        use std::os::unix::fs::PermissionsExt;

        if program == "cargo" {
            return true;
        }
        let path = Path::new(program);
        // Rustup proxies share a real path but dispatch by basename (e.g. rustc vs cargo).
        if !path.is_absolute() || path.file_name().is_none_or(|name| name != "cargo") {
            return false;
        }
        let Ok(real) = path.canonicalize() else { return false };
        let configured = self.allow.iter().filter_map(|prefix| shlex::split(prefix)?.into_iter().next());
        if configured.filter(|program| Path::new(program).is_absolute()).any(|program| {
            let path = Path::new(&program);
            path.file_name().is_some_and(|name| name == "cargo") && path.canonicalize().is_ok_and(|p| p == real)
        }) {
            return true;
        }
        let Some(path) = std::env::var_os("PATH") else { return false };
        // Relative and empty entries would resolve inside the agent's own directory.
        std::env::split_paths(&path)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join("cargo"))
            .find(|path| path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
            .and_then(|path| path.canonicalize().ok())
            .is_some_and(|path| path == real)
    }
}

#[cfg(feature = "native")]
fn assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

#[cfg(feature = "native")]
fn toolchain(word: &str) -> bool {
    word.starts_with(|c: char| c.is_ascii_alphanumeric())
        && word.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// A deliberately small shell grammar: words with quotes, joined only by plain `&&` or `;`.
/// Keep raw words so quoted assignment names cannot become shell environment prefixes.
#[cfg(feature = "native")]
fn shell_commands(command: &str) -> Option<Vec<Vec<&str>>> {
    // Keep refusing expansions and redirections even inside quotes. Reject controls (including
    // newlines) before parsing, so heredocs and line continuations cannot hide commands.
    if command.chars().any(|c| {
        (c.is_control() && c != '\t')
            || matches!(c, '|' | '<' | '>' | '`' | '$' | '*' | '?' | '[' | ']' | '{' | '}' | '~' | '(' | ')' | '\\')
    }) {
        return None;
    }
    let mut commands = Vec::new();
    let mut words = Vec::new();
    let mut start = None;
    let mut quote = None;
    let has_hash = command.contains('#');
    let mut chars = command.char_indices();
    while let Some((index, c)) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        if matches!(c, '\'' | '"') {
            start.get_or_insert(index);
            quote = Some(c);
        } else if matches!(c, ' ' | '\t' | ';' | '&') {
            if let Some(start) = start.take() {
                words.push(&command[start..index]);
            }
            if matches!(c, ';' | '&') {
                if c == '&' && chars.next().is_none_or(|(_, c)| c != '&') {
                    return None;
                }
                if words.is_empty() {
                    return None;
                }
                commands.push(std::mem::take(&mut words));
            }
        } else if c == '#' {
            // Only a hash at the start of a word begins a shell comment.
            start?;
        } else {
            start.get_or_insert(index);
        }
    }
    if quote.is_some() {
        return None;
    }
    if let Some(start) = start {
        words.push(&command[start..]);
    }
    // A literal hash, including a quoted one, is only allowed in a standalone command.
    if words.is_empty() || (has_hash && !commands.is_empty()) {
        return None;
    }
    commands.push(words);
    Some(commands)
}

#[cfg(feature = "native")]
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

fn strip_box_frame(line: &str) -> &str {
    let line = line.trim();
    let line = line.strip_prefix('│').unwrap_or(line);
    let line = line.strip_suffix('│').unwrap_or(line);
    line.trim()
}

/// What follows the prompt symbol on a composer line: `›`, `❯` or `>`, or the `→` inside the box Cursor draws
/// around its composer. Cursor's decision rows start with `→` too, but outside any box.
fn composer_text(line: &str) -> Option<&str> {
    let text = strip_box_frame(line);
    text.strip_prefix(['›', '❯', '>'])
        .or_else(|| line.trim_start().starts_with('│').then(|| text.strip_prefix('→')).flatten())
}

/// A numbered selection or an approval menu is open in place of the composer.
pub fn menu_visible(screen: &str) -> bool {
    if let Some(prompt) = screen.lines().rev().find_map(composer_text)
        && !prompt
            .trim()
            .split_once('.')
            .is_some_and(|(number, _)| !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()))
    {
        return false;
    }
    let compact: String = screen.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect();
    compact.contains("pressentertoconfirmoresctocancel")
        || compact.contains("usearrowkeystonavigate,entertoselect")
        || compact.contains("↑/↓tonavigate·entertoselect")
        || screen.lines().any(|line| {
            let line = strip_box_frame(line);
            if line.starts_with("▶ [") {
                return true;
            }
            let Some(choice) = line.strip_prefix(['›', '❯', '>']) else { return false };
            let Some((number, _)) = choice.trim().split_once('.') else { return false };
            !number.is_empty() && number.chars().all(|c| c.is_ascii_digit())
        })
}

/// A permission request or a known setup screen that an agent waits on.
pub fn first_run_prompt(screen: &str) -> bool {
    command_prompt(screen_tail(screen)) || first_run_hint(screen).is_some()
}

fn screen_tail(screen: &str) -> &str {
    let screen = screen.trim_end();
    let start = screen.match_indices('\n').rev().nth(11).map_or(0, |(at, _)| at + 1);
    &screen[start..]
}

pub fn first_run_menu_visible(screen: &str) -> bool {
    menu_visible(screen_tail(screen))
}

/// What the chief's status says while a menu of its own waits in its terminal.
pub const QUICK_ANSWER: &str = "needs a quick answer";
const USAGE_LIMIT: &str = "Usage limit reached. Open the chief terminal to see when it resets.";
const RESUME_HOW: &str = "Choose in the chief terminal whether to resume from a summary or the full session.";
const RESUME_WHERE: &str = "Choose a working directory in the chief terminal to resume.";

/// Whether the chief's status hint stands for a question its own terminal asks and takes the answer to.
pub fn answered_in_terminal(hint: &str) -> bool {
    matches!(hint, QUICK_ANSWER | USAGE_LIMIT | RESUME_HOW | RESUME_WHERE | FOLDER_ACCESS_HINT)
}

/// Provider menus that can interrupt a turn, or follow one, without a permission or stop hook.
pub fn chief_menu_hint(screen: &str) -> Option<&'static str> {
    let tail = screen_tail(screen);
    let footer = tail.lines().last()?.trim().to_ascii_lowercase();
    if footer.contains("enter to confirm")
        && footer.contains("esc to cancel")
        && tail.contains("Stop and wait for limit to reset")
        && tail.contains("Wait here, then continue automatically")
    {
        return Some(USAGE_LIMIT);
    }
    if footer.contains("enter to confirm")
        && tail.contains("Resume from summary")
        && tail.contains("Resume full session as-is")
    {
        return Some(RESUME_HOW);
    }
    if footer.contains("enter continue")
        && footer.contains("esc use session")
        && tail.contains("Use current directory (")
        && tail.contains("Use session directory (")
    {
        return Some(RESUME_WHERE);
    }
    // Claude Code offers things of its own once a turn is over, under its composer: a tip, a survey, a setting.
    (footer.contains("enter to confirm") && footer.contains("esc to cancel") && menu_visible(tail))
        .then_some(QUICK_ANSWER)
}

/// Claude Code's folder trust, as it asks since 2.1.330: its answers are not numbered, and No is the first.
fn claude_folder_access_visible(screen: &str) -> bool {
    let mut lines = screen.lines().rev().map(str::trim).filter(|line| !line.is_empty());
    lines.next().is_some_and(|footer| footer.to_ascii_lowercase().contains("enter to confirm"))
        && lines.take(3).any(|line| line.trim_start_matches(['❯', ' ']) == "Yes, I trust this folder")
}

pub const FOLDER_ACCESS_HINT: &str = "waiting for folder access";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderTrustDecision {
    Trust,
    DontTrust,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderAccess {
    Trust,
    Restricted,
    Exit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderTrustMenu {
    body: Vec<String>,
    choices: Vec<String>,
    focused: usize,
    trust: (usize, FolderAccess),
    refusal: (usize, FolderAccess),
}

impl FolderTrustMenu {
    pub fn is_claude(&self) -> bool {
        self.body[0] == "Accessing workspace:"
    }

    pub fn same(&self, other: &Self) -> bool {
        self.body == other.body && self.choices == other.choices
    }

    fn target(&self, decision: FolderTrustDecision) -> (usize, FolderAccess) {
        match decision {
            FolderTrustDecision::Trust => self.trust,
            FolderTrustDecision::DontTrust => self.refusal,
        }
    }

    pub fn access(&self, decision: FolderTrustDecision) -> FolderAccess {
        self.target(decision).1
    }

    pub fn focused_on(&self, decision: FolderTrustDecision) -> bool {
        self.focused == self.target(decision).0
    }

    pub fn navigation(&self, decision: FolderTrustDecision) -> Vec<u8> {
        let target = self.target(decision).0;
        let arrow = if target < self.focused { b"\x1b[A" } else { b"\x1b[B" };
        arrow.repeat(target.abs_diff(self.focused))
    }
}

/// Recognize a folder prompt for status and input gating, even if part of Codex's menu is clipped.
pub fn folder_trust_visible(screen: &str) -> bool {
    codex_folder_access_visible(screen) || claude_folder_access_visible(screen)
}

/// A complete startup menu, with one unambiguous focused choice and known answers.
pub fn folder_trust_menu(screen: &str) -> Option<FolderTrustMenu> {
    let screen = if claude_folder_access_visible(screen) { screen } else { active_menu_rows(screen) };
    let mut rows: Vec<_> = screen.lines().map(str::trim).filter(|row| !row.is_empty()).collect();
    let footer = rows.pop()?;
    let (heading, first_choice, claude) = match footer {
        "Enter to confirm · Esc to cancel" => (
            "Accessing workspace:",
            rows.iter().rposition(|row| row.trim_start_matches('❯').trim_start() == "No, exit")?,
            true,
        ),
        "enter continue · esc quit" | "enter continue · esc back" => {
            let start = rows.iter().rposition(|row| choice(row).is_none())? + 1;
            (FOLDER_ACCESS_TITLE, start, false)
        }
        _ => return None,
    };
    let heading = rows.iter().rposition(|row| *row == heading)?;
    if first_choice <= heading + 1
        || (!claude
            && (!rows[heading + 1].starts_with('/')
                || rows[heading + 1..first_choice].iter().any(|row| choice(row).is_some())))
    {
        return None;
    }
    let mut focused = None;
    let mut choices = Vec::new();
    for (index, row) in rows[first_choice..].iter().enumerate() {
        if row.starts_with(if claude { '❯' } else { '›' }) && focused.replace(index).is_some() {
            return None;
        }
        let label = if claude {
            row.trim_start_matches('❯').trim_start()
        } else {
            let (number, label) = choice(row)?;
            if number.parse::<usize>() != Ok(index + 1) {
                return None;
            }
            label
        };
        choices.push(label.to_owned());
    }
    let find = |labels: &[&str], access| {
        let mut hits = choices.iter().enumerate().filter(|(_, label)| labels.contains(&label.as_str()));
        let (index, _) = hits.next()?;
        hits.next().is_none().then_some((index, access))
    };
    let (trust, refusal) = if claude {
        if choices.len() != 2 {
            return None;
        }
        (find(&["Yes, I trust this folder"], FolderAccess::Trust)?, find(&["No, exit"], FolderAccess::Exit)?)
    } else {
        let trusted = find(&["Trust and continue"], FolderAccess::Trust);
        let restricted = find(&["Open restricted"], FolderAccess::Restricted);
        let exit = find(&["Back to Agent Command Center", "Quit"], FolderAccess::Exit);
        (trusted.or(restricted)?, trusted.and(restricted).or(exit)?)
    };
    let mut body: Vec<_> = rows[heading..first_choice].iter().map(|row| (*row).to_owned()).collect();
    body.push(footer.to_owned());
    Some(FolderTrustMenu { body, choices, focused: focused?, trust, refusal })
}

/// A CLI's own offer to update itself before it starts, which the chief or the user answers in its terminal.
pub const UPDATE_OFFERED: &str = "offers an update";

/// The known setup screens that can stop an agent before its hooks report a turn.
pub fn first_run_hint(screen: &str) -> Option<&'static str> {
    if folder_trust_visible(screen) {
        return Some(FOLDER_ACCESS_HINT);
    }
    let tail = screen_tail(screen);
    // An ordinary composer below an answer means these are transcript lines, not a setup menu.
    if let Some(prompt) = tail.lines().rev().find_map(composer_text)
        && !prompt.trim().split_once('.').is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    let mut setup = false;
    for line in tail.lines() {
        let line = strip_box_frame(line);
        let line = line.trim_start_matches(['›', '❯', '>', '▶', '⚠']).trim();
        let line = line
            .split_once('.')
            .filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
            .map_or(line, |(_, text)| text.trim());
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_lowercase();
        // Codex's "Update available · 0.171.0 → 0.172.0", and its older "✨ Update available! …".
        if line.contains("update available") {
            return Some(UPDATE_OFFERED);
        }
        match line.trim_end_matches(':') {
            "sign in with chatgpt"
            | "sign in with chatgpt to use codex as part of your paid plan"
            | "sign in to continue"
            | "select login method"
            | "select a login method" => return Some("needs sign-in"),
            // Cursor's workspace trust, and Copilot's folder trust. Copilot asks Claude's question ("do you trust the
            // files in this folder?"), which stays a first-run screen below; its title and its remember choice
            // tell the two apart.
            "workspace trust required"
            | "do you trust the contents of this directory?"
            | "[a] trust this workspace"
            | "confirm folder trust"
            | "yes, and remember this folder for future sessions" => return Some(FOLDER_ACCESS_HINT),
            "mcp server approval required"
            | "the following mcp servers need to be approved"
            | "[a] approve all servers" => return Some("MCP approval needed"),
            "trust this folder?"
            | "trust and continue"
            | "do you trust the files in this folder?"
            | "skip until next version"
            | "press enter to continue"
            | "choose theme"
            | "choose a theme"
            | "choose the theme"
            | "select theme"
            | "select a theme"
            | "review hooks"
            | "hooks need review"
            | "choose the text style that looks best with your terminal" => setup = true,
            _ => {}
        }
    }
    setup.then_some("first-run setup")
}

/// Titles of Cursor CLI's approval prompts (2026.11).
const CURSOR_DECISIONS: [&str; 9] = [
    "Run this command?",
    "Run this command outside the sandbox?",
    "Run this MCP tool?",
    "Delete this file?",
    "Write to this file?",
    "Read this file?",
    "Allow this web search?",
    "Allow this web fetch?",
    "Proceed with this edit?",
];

/// What Cursor CLI's composer says in place of its placeholder while it waits for an answer.
const CURSOR_WAITING: [&str; 4] = [
    "waitingfordecision(y/n/p)",
    "approvemodeswitch(y/n)",
    "answerquestions(entertoselect/next,esctoskip)",
    // After "Skip & tell the agent what to do instead": what is typed now goes with the refusal.
    "telltheagentwhattodoinstead(entertosend",
];

/// A menu Copilot or Cursor CLI drew at the bottom of `screen`: a permission or trust question, or any other
/// selection. Neither asks through a hook ATOMICS can answer, and Enter (or Cursor's `y`) answers what they show,
/// so typed input waits until the person answers it. Copilot ends every selection with "↑/↓ to navigate · enter
/// to select"; Cursor lists its choices under the question, the focused one marked `→` outside its composer box.
pub fn cli_menu_visible(screen: &str) -> bool {
    let screen = screen.trim_end();
    let start = screen.match_indices('\n').rev().nth(19).map_or(0, |(at, _)| at + 1);
    let tail = &screen[start..];
    let compact: String = tail.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect();
    if ["↑/↓tonavigate·entertoselect", "↑/↓toselect·entertoconfirm"].iter().any(|hint| compact.contains(hint))
        || CURSOR_WAITING.iter().any(|hint| compact.contains(hint))
    {
        return true;
    }
    // Lines above the last composer are transcript.
    let lines: Vec<&str> = tail.lines().collect();
    let below = lines
        .iter()
        .rposition(|line| composer_text(line).is_some_and(|text| choice(text).is_none()))
        .map_or(0, |at| at + 1);
    let lines = &lines[below..];
    let rows: Vec<&str> = lines.iter().map(|line| strip_box_frame(line)).filter(|row| !row.is_empty()).collect();
    // Copilot: "Do you want to run this command?" over "❯ 1. Yes".
    let copilot = rows.iter().any(|row| row.starts_with("Do you want to ") && row.ends_with('?'))
        && rows.iter().filter_map(|row| choice(row)).any(|(number, label)| number == "1" && label.starts_with("Yes"));
    // Cursor: "Run this command?" over "→ Run (once) (y)" and "Skip … (esc or n)".
    let focused = lines.iter().map(|line| line.trim()).any(|row| row.starts_with("→ ") && row.ends_with(')'));
    let cursor = focused
        && rows.iter().any(|row| {
            CURSOR_DECISIONS.contains(row)
                || (row.starts_with("Switch to ") && row.ends_with(" mode?"))
                || row.ends_with("(esc or n)")
                || row.ends_with("(n or esc)")
        });
    copilot || cursor || menu_visible(tail)
}

const CODEX_QUESTION: &str = "Would you like to run the following command?";
const CLAUDE_QUESTION: &str = "Do you want to proceed?";
const CLAUDE_TITLE: &str = "Bash command";
/// Codex's "⌃a view all": shows a command it hid or shortened.
const EXPAND: &str = "\x01";

/// The agent that drew a native command menu; each answers to its own keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKind {
    /// "Would you like to run the following command?"
    Codex,
    /// "Bash command … Do you want to proceed?"
    Claude,
    /// "Do you want to run this command?" or "Do you want to use this tool?" over "❯ 1. Yes"
    Copilot,
    /// "Run this command?" or "Run this MCP tool?" over "→ Run (once) (y)"
    Cursor,
}

impl MenuKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Copilot => "copilot",
            Self::Cursor => "cursor",
        }
    }
}

/// A command approval menu an agent drew itself, read off its screen. It is the one kind of native
/// menu ATOMICS answers without a hook request: the command it would run is on the screen, to compare
/// before every key and to echo to whoever decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeMenu {
    pub kind: MenuKind,
    /// Codex shows an arrow when some choices do not fit in its viewport.
    pub clipped: bool,
    /// The command as shown, a line per screen line; `None` while Codex hides or shortens it behind
    /// "⌃a view all". Claude draws the command's description beside it, so its lines include that.
    pub command: Option<String>,
    /// Why Codex asks, from its "Reason:"; the title Copilot gives a command, or the reason Cursor shows.
    pub reason: Option<String>,
    /// For Copilot and Cursor, the MCP tool a menu asks about; `None` for a shell command.
    pub tool: Option<String>,
    /// Copilot's and Cursor's choices as shown, which bind an answer to this very menu.
    pub choices: Vec<String>,
}

impl NativeMenu {
    /// Changed text or wrapping requires a fresh decision.
    pub fn same(&self, other: &Self) -> bool {
        self == other
    }
}

fn compact(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// A numbered menu choice, focused or not.
fn choice(line: &str) -> Option<(&str, &str)> {
    let line = line.trim().trim_start_matches(['›', '❯', '>']).trim_start();
    line.split_once(". ").filter(|(number, _)| !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()))
}

/// The command approval menu waiting at the bottom of `screen`: Codex's "Would you like to run the
/// following command?" or Claude's "Bash command … Do you want to proceed?". Menus for edits,
/// terminal input or questions are not command approvals, and neither is a menu already answered.
pub fn native_menu(screen: &str) -> Option<NativeMenu> {
    codex_menu(screen).or_else(|| claude_menu(screen))
}

pub const INCOMPLETE_MENU: &str =
    "Permission approval is pending. The menu is not fully visible. Answer it in the ATOMICS desktop window.";

const FOLDER_ACCESS_TITLE: &str = "Folder access";

/// Also recognize an unfamiliar or partly drawn folder menu for status and input gating.
pub fn codex_folder_access_visible(screen: &str) -> bool {
    let active = active_menu_rows(screen);
    let mut rows = active.lines().map(str::trim).filter(|row| !row.is_empty());
    rows.by_ref().any(|row| row == FOLDER_ACCESS_TITLE)
        && (rows.next().is_some_and(|row| row.starts_with('/')) || menu_visible(active))
}

/// The folder on a complete Codex menu with its restricted option already selected.
pub fn codex_restricted_folder(screen: &str) -> Option<Cow<'_, str>> {
    let mut rows = active_menu_rows(screen).lines().rev().filter(|row| !row.trim().is_empty());
    match (rows.next()?.trim(), rows.next()?.trim(), rows.next()?.trim()) {
        ("enter continue · esc quit", "2. Quit", "› 1. Open restricted")
        | ("enter continue · esc back", "2. Back to Agent Command Center", "› 1. Open restricted") => {}
        _ => return None,
    }
    let mut body = Vec::new();
    for row in rows {
        if row.trim() == FOLDER_ACCESS_TITLE {
            let mut rows = body.into_iter().rev();
            let first: &str = rows.next()?;
            let mut folder = Cow::Borrowed(first.trim());
            if !folder.starts_with('/') {
                return None;
            }
            // Codex wraps the path in its own paragraph, above the restricted-access disclosure.
            for row in rows.take_while(|row| !row.trim_start().starts_with("Config, hooks, and exec policies")) {
                folder.to_mut().push_str(row.strip_prefix("  ").unwrap_or(row).trim_end());
            }
            return Some(folder);
        }
        if choice(row).is_some() {
            return None;
        }
        body.push(row);
    }
    None
}

/// Only inspect text below the most recent composer. A previous turn's menu can
/// remain in the transcript above it.
fn active_menu_rows(screen: &str) -> &str {
    let mut after_composer = 0;
    let mut end = 0;
    for row in screen.split_inclusive('\n') {
        end += row.len();
        let line = row.trim();
        if line.starts_with(['›', '❯', '>']) && choice(line).is_none() {
            after_composer = end;
        }
    }
    &screen[after_composer..]
}

/// Recognize permission requests, including partial draws where the command or focus marker is missing.
pub fn command_prompt(screen: &str) -> bool {
    let active = active_menu_rows(screen);
    if active.is_empty() {
        return false;
    }
    let joined = compact(active);
    if joined.contains("Wouldyouliketorunthefollowingcommand?")
        || joined.contains("Doyouwanttoproceed?")
        || joined.contains("Wouldyouliketomakethefollowingedits?")
        || joined.contains("Wouldyouliketograntthesepermissions?")
        || joined.contains("Wouldyouliketosendinputto")
        || joined.contains("Doyouwanttoapprovenetworkaccessto")
        || joined.contains("Allowthisrequest?")
        || joined.contains("Pressentertoconfirmoresctocancel")
        || codex_folder_access_visible(active)
        || active.lines().any(|line| line.trim_end().ends_with(" needs your approval."))
    {
        return true;
    }
    active.lines().filter_map(choice).any(|(_, label)| {
        matches!(label, "Yes" | "No" | "Allow" | "Allow once" | "Deny")
            || [
                "Yes, proceed",
                "Yes, and ",
                "Yes, grant ",
                "Yes, just this once",
                "Yes, provide the requested info",
                "No, and tell Codex",
                "No, continue without permissions",
            ]
            .iter()
            .any(|prefix| label.starts_with(prefix))
    })
}

fn clipped_choices(screen: &str) -> bool {
    let rows = active_menu_rows(screen);
    rows.lines().any(|line| line.trim() == "↓")
        && rows.lines().any(|line| choice(line).is_some())
        && compact(rows).contains("Pressentertoconfirm")
        && command_prompt(rows)
}

impl NativeMenu {
    /// Only the single command row and the ordinary one-time Yes choice can be read unambiguously.
    /// Claude mixes descriptions and commands under the same heading. Copilot and Cursor show a shell
    /// command whole when it fits on one row, with their choices below it.
    pub fn single_command(&self, screen: &str) -> bool {
        if matches!(self.kind, MenuKind::Copilot | MenuKind::Cursor) {
            return !self.clipped
                && self.tool.is_none()
                && self.command.as_deref().is_some_and(|command| {
                    !command.is_empty() && !command.contains(['\n', '…']) && !command.chars().any(char::is_control)
                })
                && cli_native_menu(screen).is_some_and(|visible| visible.same(self));
        }
        let Some(command) = self.command.as_deref() else { return false };
        let Some((_, body)) = screen.split_once(CODEX_QUESTION) else { return false };
        if self.clipped
            || clipped_choices(screen)
            || self.kind != MenuKind::Codex
            || body.contains(CODEX_QUESTION)
            || command.is_empty()
            || command.contains(['\n', '…'])
            || command.chars().any(char::is_control)
        {
            return false;
        }
        if body.lines().last().map(str::trim) != Some("Press enter to confirm or esc to cancel")
            || body.contains(CLAUDE_QUESTION)
        {
            return false;
        }
        let choices = body.lines().filter_map(choice);
        let mut rows = body.lines().skip_while(|line| !line.trim_start().starts_with('$'));
        rows.next().and_then(|line| line.trim().strip_prefix("$ ")) == Some(command)
            && rows.find(|line| !line.trim().is_empty()).and_then(choice) == Some(("1", "Yes, proceed (y)"))
            && choices.enumerate().all(|(i, (n, _))| n.parse::<usize>() == Ok(i + 1))
            && !body.contains("⌃a view all")
    }
}

/// The keys that answer `menu` with `decision` while `screen` still shows that very menu, or that
/// first expand the command it hides. `None` once the screen shows anything else.
pub fn native_key(screen: &str, menu: &NativeMenu, decision: Decision) -> Option<String> {
    if matches!(menu.kind, MenuKind::Copilot | MenuKind::Cursor) {
        return cli_key(screen, menu, decision);
    }
    let visible = native_menu(screen).filter(|visible| visible.same(menu))?;
    if visible.clipped {
        return None;
    }
    if visible.single_command(screen) {
        return Some(if decision == Decision::Deny { "\x1b" } else { "1\r" }.into());
    }
    match &visible.command {
        None => Some(EXPAND.into()),
        Some(command) => menu_key(screen, Some(command), decision),
    }
}

/// [`native_menu`], or for a Copilot or Cursor terminal its CLI's own command or MCP tool menu.
pub fn native_menu_in(screen: &str, cli: bool) -> Option<NativeMenu> {
    if cli { cli_native_menu(screen) } else { native_menu(screen) }
}

const COPILOT_COMMAND: &str = "Do you want to run this command?";
const COPILOT_TOOL: &str = "Do you want to use this tool?";
const COPILOT_DENY: &str = "No, and tell Copilot what to do differently";
const CURSOR_COMMAND: [&str; 2] = ["Run this command?", "Run this command outside the sandbox?"];
const CURSOR_TOOL: &str = "Run this MCP tool?";
/// Cursor's composer after "Skip & tell the agent what to do instead": Enter with nothing typed skips the reason.
pub const CURSOR_REJECTION_REASON: &str = "Tell the agent what to do instead";

/// The command or MCP tool menu Copilot or Cursor CLI drew at the bottom of `screen`, with the text it asks
/// about and its choices. Trust, sign-in, MCP-server, edit and other menus are none: they stay the person's.
pub fn cli_native_menu(screen: &str) -> Option<NativeMenu> {
    copilot_menu(screen).or_else(|| cursor_menu(screen))
}

/// Copilot (1.0): a rounded dialog titled with what the command is for, the command in a box of its own, the
/// question, then numbered choices, Yes first and "No, and tell Copilot what to do differently" last, and the
/// "↑/↓ to navigate · enter to select" footer. An MCP tool's box holds its arguments under the tool's title.
fn copilot_menu(screen: &str) -> Option<NativeMenu> {
    let lines: Vec<&str> = screen.trim_end().lines().collect();
    let question = lines.iter().rposition(|line| matches!(strip_box_frame(line), COPILOT_COMMAND | COPILOT_TOOL))?;
    let mut choices = Vec::new();
    let mut complete = false;
    for line in &lines[question + 1..] {
        let row = strip_box_frame(line);
        if row.is_empty() {
            continue;
        }
        if row.starts_with('╰') {
            break;
        }
        if row.starts_with("↑/↓ to navigate") {
            complete = true;
            break;
        }
        match choice(row) {
            Some((number, label)) if number.parse::<usize>() == Ok(choices.len() + 1) => {
                choices.push(label.trim().to_string());
            }
            Some(_) => return None,
            // A long choice wraps onto the next row.
            None => {
                let last: &mut String = choices.last_mut()?;
                last.push(' ');
                last.push_str(row);
            }
        }
    }
    if choices.len() < 2 || choices[0] != "Yes" || !choices.last()?.starts_with(COPILOT_DENY) {
        return None;
    }
    // The box right above the question, between the dialog's title and it.
    let bottom = lines[..question].iter().rposition(|line| !strip_box_frame(line).is_empty())?;
    if !strip_box_frame(lines[bottom]).starts_with('╰') {
        return None;
    }
    let top = lines[..bottom].iter().rposition(|line| strip_box_frame(line).starts_with('╭'))?;
    let shown: Vec<&str> = lines[top + 1..bottom].iter().map(|line| strip_box_frame(strip_box_frame(line))).collect();
    let dialog = lines[..top].iter().rposition(|line| line.trim_start().starts_with('╭'));
    let title = dialog.and_then(|at| {
        lines[at + 1..top].iter().map(|line| strip_box_frame(line)).find(|row| !row.is_empty() && !row.starts_with('─'))
    });
    let tool = strip_box_frame(lines[question]) == COPILOT_TOOL;
    Some(NativeMenu {
        kind: MenuKind::Copilot,
        clipped: !complete || dialog.is_none(),
        command: Some(shown.join("\n")).filter(|command| !command.is_empty()),
        reason: title.filter(|_| !tool).map(str::to_string),
        tool: tool.then(|| title.unwrap_or("MCP tool").to_string()),
        choices,
    })
}

/// Cursor (2026.11): under a rule, `$ command in folder` (or an MCP tool's `server: tool` and arguments), the
/// question, the reason it asks, then its choices, the focused one marked `→`, each with its key: "(y)" runs it
/// once and "(esc or n)" skips it.
fn cursor_menu(screen: &str) -> Option<NativeMenu> {
    let lines: Vec<&str> = screen.trim_end().lines().collect();
    let question =
        lines.iter().rposition(|line| CURSOR_COMMAND.contains(&line.trim()) || line.trim() == CURSOR_TOOL)?;
    let mut reason = Vec::new();
    let mut choices = Vec::new();
    let mut focused = 0;
    for line in &lines[question + 1..] {
        let row = line.trim();
        if row.is_empty() {
            continue;
        }
        let (focus, label) = match row.strip_prefix("→ ") {
            Some(label) => (true, label),
            None => (false, row),
        };
        let hint = label.rfind(" (").filter(|_| label.ends_with(')')).map(|at| &label[at + 1..]);
        match hint {
            // Every choice ends with its key; rows without one are the reason above them, or text below.
            Some(hint) if focus || !choices.is_empty() || hint == "(y)" => {
                focused += usize::from(focus);
                choices.push(label.to_string());
            }
            _ if choices.is_empty() && !focus => reason.push(row),
            _ => {}
        }
    }
    if focused != 1
        || !choices.iter().any(|choice| choice.ends_with(" (y)"))
        || !choices.iter().any(|choice| choice.ends_with("(esc or n)"))
    {
        return None;
    }
    // The preview runs from the rule above it to the question.
    let rule = lines[..question].iter().rposition(|line| {
        let line = line.trim();
        !line.is_empty() && line.chars().all(|c| c == '─')
    })?;
    let preview: Vec<&str> = lines[rule + 1..question]
        .iter()
        .map(|line| line.trim())
        .filter(|row| !row.is_empty() && !(row.starts_with("Approval ") && row.contains(" of ")))
        .collect();
    let tool = lines[question].trim() == CURSOR_TOOL;
    let (command, tool) = if tool {
        let (name, args) = preview.split_first()?;
        (Some(args.join("\n")).filter(|args| !args.is_empty()), Some(name.to_string()))
    } else {
        let (first, rest) = preview.split_first()?;
        let first = first.strip_prefix('$')?.trim_start();
        let shown = std::iter::once(first).chain(rest.iter().copied()).collect::<Vec<_>>().join("\n");
        // Cursor says where it runs after the command: "… in ~/repo".
        (Some(shown.rsplit_once(" in ").map_or(shown.as_str(), |(command, _)| command).to_string()), None)
    };
    Some(NativeMenu {
        kind: MenuKind::Cursor,
        clipped: false,
        command,
        reason: Some(reason.join(" ")).filter(|reason| !reason.is_empty()),
        tool,
        choices,
    })
}

/// Copilot: Enter while its focus is on "1. Yes", Escape for no. Cursor: its own `y` and `n` keys. "Always"
/// choices are never taken: a menu read off the screen answers only itself.
fn cli_key(screen: &str, menu: &NativeMenu, decision: Decision) -> Option<String> {
    let visible = cli_native_menu(screen).filter(|visible| visible.same(menu))?;
    if visible.clipped {
        return None;
    }
    match (visible.kind, decision) {
        (MenuKind::Copilot, Decision::Deny) => Some("\x1b".into()),
        (MenuKind::Copilot, _) => {
            let lines: Vec<&str> = screen.trim_end().lines().collect();
            let question =
                lines.iter().rposition(|line| matches!(strip_box_frame(line), COPILOT_COMMAND | COPILOT_TOOL))?;
            let focused =
                lines[question + 1..].iter().map(|line| strip_box_frame(line)).find(|row| row.starts_with('❯'))?;
            (choice(focused) == Some(("1", "Yes"))).then(|| "\r".into())
        }
        (MenuKind::Cursor, Decision::Deny) => Some("n".into()),
        (MenuKind::Cursor, _) => Some("y".into()),
        _ => None,
    }
}

fn codex_menu(screen: &str) -> Option<NativeMenu> {
    let clipped = clipped_choices(screen);
    let menu = match screen.rsplit_once(CODEX_QUESTION) {
        Some((_, body)) => body,
        None if clipped => active_menu_rows(screen),
        None => return None,
    };
    if !clipped && !menu_visible(menu) {
        return None;
    }
    let expandable = menu.contains("⌃a view all");
    let command = match codex_shown(menu) {
        Some(shown) if !shown.contains("[…") => Some(shown),
        Some(_) if clipped || expandable => None,
        None if clipped || expandable && menu.contains("Reason:") => None,
        _ => return None,
    };
    let reason = menu.lines().position(|line| line.trim_start().starts_with("Reason:")).map(|at| {
        let lines = menu.lines().skip(at).take_while(|line| {
            let line = line.trim();
            !line.is_empty() && !line.starts_with('$') && !line.starts_with("[…") && choice(line).is_none()
        });
        let reason = lines.map(str::trim).collect::<Vec<_>>().join(" ");
        reason.trim_start_matches("Reason:").trim().to_string()
    });
    Some(NativeMenu {
        kind: MenuKind::Codex,
        clipped,
        command,
        reason: reason.filter(|reason| !reason.is_empty()),
        tool: None,
        choices: Vec::new(),
    })
}

/// The command Codex shows after its `$`, a line per screen line, or `None` when it shows none or
/// the screen cannot tell where it ends. Codex lists its choices right below the command (0.165)
/// or after a blank row. A blank row inside the command, or a line of it that reads as a choice
/// (`1. text`), looks like the end of the command: the part above it would be reviewed and
/// approved while Codex runs all of it. Such a command is expanded or left to the terminal.
fn codex_shown(menu: &str) -> Option<String> {
    let line = menu.lines().find(|line| line.trim_start().starts_with('$'))?;
    let shown = menu[menu.find(line)?..].trim_start().strip_prefix('$')?;
    let lines: Vec<&str> = shown.lines().collect();
    // The menu's own first choice is the last one numbered 1: the choices come last.
    let menu_start = lines.iter().rposition(|line| choice(line).is_some_and(|(number, _)| number == "1"))?;
    let end = lines[..menu_start].iter().rposition(|line| !line.trim().is_empty()).map_or(0, |last| last + 1);
    let command = &lines[..end];
    if command.iter().any(|line| line.trim().is_empty() || choice(line).is_some()) {
        return None;
    }
    Some(command.iter().map(|line| line.trim()).collect::<Vec<_>>().join("\n").trim().to_string())
}

fn claude_menu(screen: &str) -> Option<NativeMenu> {
    let (body, _) = claude_parts(screen)?;
    Some(NativeMenu {
        kind: MenuKind::Claude,
        clipped: false,
        command: Some(body.join("\n")),
        reason: None,
        tool: None,
        choices: Vec::new(),
    })
}

/// Claude's command menu: the lines under its "Bash command" title, and its choices, Yes first.
fn claude_parts(screen: &str) -> Option<(Vec<&str>, Vec<&str>)> {
    let lines: Vec<&str> = screen.lines().collect();
    let question = lines.iter().rposition(|line| line.trim() == CLAUDE_QUESTION)?;
    let choices = lines[question + 1..].to_vec();
    if !menu_visible(&choices.join("\n")) {
        return None;
    }
    let first = choices.iter().find(|line| !line.trim().is_empty())?;
    if !choice(first).is_some_and(|(number, label)| number == "1" && label.trim().eq_ignore_ascii_case("yes")) {
        return None;
    }
    // "Bash command", or with a note such as "Bash command (unsandboxed)".
    let title = lines[..question].iter().rposition(|line| claude_title(line))?;
    let mut body: Vec<&str> =
        lines[title + 1..question].iter().map(|line| line.trim()).filter(|l| !l.is_empty()).collect();
    // Claude Code advertises its auto mode above the command, which is ruled off; neither is part of what is asked.
    if body.first().is_some_and(|line| line.starts_with("Tip: auto mode")) {
        let tip = body.iter().take(3).position(|line| line.ends_with("below")).map_or(1, |last| last + 1);
        body.drain(..tip);
    }
    body.retain(|line| line.chars().any(|c| c != '╌'));
    if body.is_empty() || body.iter().any(|line| choice(line).is_some() || line.starts_with(['›', '❯', '>'])) {
        return None;
    }
    Some((body, choices))
}

fn claude_title(line: &str) -> bool {
    line.trim()
        .strip_prefix(CLAUDE_TITLE)
        .is_some_and(|note| note.is_empty() || note.strip_prefix(" (").is_some_and(|note| note.ends_with(')')))
}

fn claude_key(choices: &[&str], decision: Decision) -> Option<String> {
    match decision {
        // Enter takes the focused choice, which must be the plain Yes.
        Decision::Allow | Decision::AllowAlways => {
            let focused = choices.iter().find(|line| line.trim_start().starts_with('❯'))?;
            choice(focused).is_some_and(|(_, label)| label.trim().eq_ignore_ascii_case("yes")).then(|| "\r".into())
        }
        Decision::Deny => choices
            .iter()
            .any(|line| {
                let line = line.to_lowercase();
                line.contains("(esc)") || line.contains("esc to cancel")
            })
            .then(|| "\x1b".into()),
    }
}

/// Match a complete native menu command, expanding hidden or truncated commands first.
pub fn menu_key(screen: &str, command: Option<&str>, decision: Decision) -> Option<String> {
    if clipped_choices(screen) {
        return None;
    }
    let command = command?;
    let Some((_, menu)) = screen.rsplit_once(CODEX_QUESTION) else {
        let (body, choices) = claude_parts(screen)?;
        return if !command.is_empty() && body.join("\n") == command { claude_key(&choices, decision) } else { None };
    };
    if !menu_visible(menu) {
        return None;
    }
    let Some(shown) = codex_shown(menu) else {
        return (menu.contains("Reason:") && menu.contains("⌃a view all")).then(|| EXPAND.into());
    };
    let truncated_match = compact(&shown).rsplit_once("[…").is_some_and(|(prefix, marker)| {
        let lines = marker.strip_suffix("lines]⌃aviewall").or_else(|| marker.strip_suffix("lines]"));
        !prefix.is_empty()
            && compact(command).starts_with(prefix)
            && lines.is_some_and(|lines| lines.parse::<usize>().is_ok_and(|lines| lines > 0))
    });
    if truncated_match {
        return menu.contains("⌃a view all").then(|| EXPAND.into());
    }
    if shown != command {
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
    use std::path::Path;

    #[cfg(feature = "native")]
    fn request(command: &str, cwd: &Path) -> ApprovalRequest {
        ApprovalRequest {
            tool: "Bash".into(),
            command: Some(command.into()),
            cwd: cwd.into(),
            reason: None,
            input: Value::Null,
            warning: None,
        }
    }

    #[test]
    #[cfg(feature = "native")]
    fn dropping_shared_stash_entries_always_waits_and_is_never_allowed_always() {
        let dir = crate::test_support::tempdir().unwrap();
        let policy = ApprovalPolicy { allow: vec!["git".into(), "git stash".into()], env: Vec::new() };
        for command in ["git stash list", "git stash push -m ENG-123", "git stash apply 0", "git stash"] {
            let request = request(command, dir.path());
            assert!(policy.allows(&request, dir.path()), "{command}");
            assert!(request.may_allow_always(), "{command}");
        }
        for command in
            ["git stash drop", "git stash drop 1", "git stash pop", "git stash clear", "git status; git stash pop 0"]
        {
            let request = request(command, dir.path());
            assert!(!policy.allows(&request, dir.path()), "{command}");
            assert!(!request.may_allow_always(), "{command}");
        }
        let azure = ApprovalRequest { tool: "Azure REST".into(), ..request("PUT https://example.com/", dir.path()) };
        assert!(!azure.may_allow_always());
        // Only a shell command drops entries: an edit's text that says so does not.
        let edit = ApprovalRequest { tool: "Edit".into(), ..request("git stash clear", dir.path()) };
        assert!(edit.stash_drops().is_empty() && edit.may_allow_always());
    }

    #[test]
    #[cfg(feature = "native")]
    fn default_policy_allows_only_worktree_reads() {
        let dir = crate::test_support::tempdir().unwrap();
        for command in ["ls -la", "cat Cargo.toml", "rg hook src", "git status --short", "git diff", "git log -5"] {
            assert!(ApprovalPolicy::default().allows(&request(command, dir.path()), dir.path()), "{command}");
        }
    }

    #[test]
    #[cfg(feature = "native")]
    fn cargo_requires_an_explicit_policy_entry() {
        let dir = crate::test_support::tempdir().unwrap();
        for command in [
            "cargo build",
            "cargo test",
            "cargo check",
            "cargo clippy",
            "cargo fmt",
            "cargo +stable test",
            "rustup run stable cargo test",
            "RUSTC_WRAPPER= cargo test",
            "env RUSTC_WRAPPER= cargo test",
            "ls && cargo test",
        ] {
            assert!(!ApprovalPolicy::default().allows(&request(command, dir.path()), dir.path()));
        }
        assert!(
            ApprovalPolicy { allow: vec!["cargo test".into()], ..ApprovalPolicy::default() }
                .allows(&request("cargo test -p atomics-core", dir.path()), dir.path())
        );
    }

    #[test]
    #[cfg(feature = "native")]
    fn policy_judges_chains_env_and_toolchains_by_their_parts() {
        let dir = crate::test_support::tempdir().unwrap();
        let mut policy = ApprovalPolicy {
            env: vec!["RUSTC_WRAPPER=".into(), "CARGO_TERM_COLOR=always".into()],
            ..ApprovalPolicy::default()
        };
        policy.allow.extend(["cargo check", "cargo test", "cargo clippy", "cargo fmt"].map(str::to_string));
        for (command, expected) in [
            ("ls&&git status; cargo test -p atomics-core", true),
            ("cargo check && cargo test && cargo fmt --check", true),
            ("RUSTC_WRAPPER= cargo check", true),
            ("RUSTC_WRAPPER='' cargo check", true),
            ("RUSTC_WRAPPER= CARGO_TERM_COLOR=always cargo test", true),
            ("env RUSTC_WRAPPER= cargo test", true),
            ("env 'RUSTC_WRAPPER=' cargo test", true),
            ("RUSTC_WRAPPER= env CARGO_TERM_COLOR=always cargo test", true),
            ("cargo +stable check", true),
            ("cargo +nightly-2026-10-01 clippy -- -D warnings", true),
            ("rustup run stable cargo test -p atomics-core", true),
            ("env RUSTC_WRAPPER= rustup run stable cargo test", true),
            ("RUSTC_WRAPPER= cargo +stable test && env RUSTC_WRAPPER= cargo fmt --check", true),
            ("atomics queue --class test -- env RUSTC_WRAPPER= cargo +stable test", true),
            ("env RUSTC_WRAPPER= atomics queue --class=test -- rustup run stable cargo test", true),
            ("atomics queue -- cargo test; git status", true),
            ("rg 'a && b' src && git status", true),
            ("rg \"a; b\" src; ls", true),
            ("rg a'&&'b src", true),
            ("cat 'file with spaces'", true),
            ("\tgit status\t&&\tls", true),
            ("", false),
            (" ", false),
            ("cargo check && rm file", false),
            ("rm file; cargo check", false),
            ("cargo test && cargo publish", false),
            ("cargo test; unknown-program", false),
            ("cargo test && env RUSTC=evil cargo check", false),
            ("cargo test &&", false),
            ("; cargo test", false),
            ("cargo test;;ls", false),
            ("cargo test &&& ls", false),
            ("cargo test & ls", false),
            ("cargo test || ls", false),
            ("cargo test | cat", false),
            ("cargo test > output", false),
            ("cargo test 2>&1", false),
            ("cargo test < input", false),
            ("cargo test\nls", false),
            ("cargo test\rls", false),
            ("cargo test\0", false),
            ("cat <<'EOF'\nls && rm file\nEOF", false),
            ("cat <<'EOF' && ls", false),
            ("cargo test # && rm file", false),
            ("cargo test $(ls)", false),
            ("cargo test `ls`", false),
            ("cargo test $ARGS", false),
            ("cat *", false),
            ("cat file?", false),
            ("cat [ab]", false),
            ("cat {a,b}", false),
            ("cat ~/file", false),
            ("ls \\\n&& ls", false),
            ("rg 'unterminated && ls", false),
            ("rg \"unterminated; ls", false),
            ("rg '&&' /etc/passwd", false),
            ("rg '&&' src && rm file", false),
            ("cat ../secret && ls", false),
            ("ls; git diff --ext-diff", false),
            ("RUSTC_WRAPPER=evil cargo test", false),
            ("RUSTC_WRAPPER= evil", false),
            ("RUSTC_WRAPPER=", false),
            ("'RUSTC_WRAPPER=' cargo test", false),
            ("RUSTC_WRAPPER''= cargo test", false),
            ("RUSTC=arbitrary cargo test", false),
            ("PATH=bin cargo test", false),
            ("DYLD_INSERT_LIBRARIES=evil cargo test", false),
            ("LD_PRELOAD=evil cargo test", false),
            ("CARGO_TERM_COLOR=never cargo test", false),
            ("env RUSTC_WRAPPER=evil cargo test", false),
            ("env RUSTC_WRAPPER= LD_PRELOAD=evil cargo test", false),
            ("env PATH=bin cargo test", false),
            ("env -i RUSTC_WRAPPER= cargo test", false),
            ("env -- RUSTC_WRAPPER= cargo test", false),
            ("env RUSTC_WRAPPER=", false),
            ("atomics queue -- RUSTC_WRAPPER= cargo test", false),
            ("atomics queue --class qa -- env RUSTC_WRAPPER= sh -c cargo", false),
            ("cargo +stable publish", false),
            ("cargo + test", false),
            ("cargo +../evil test", false),
            ("rustup run ../evil cargo test", false),
            ("rustup run --install cargo test", false),
            ("rustup run stable rustc test", false),
            ("rustup run stable sh -c cargo", false),
            ("rustup run stable", false),
            ("rustup default stable", false),
            ("./cargo test", false),
            ("/does-not-exist/cargo test", false),
            ("cargo test --manifest-path ../Cargo.toml", false),
            ("cargo +stable test --manifest-path=../Cargo.toml", false),
            ("cargo test --manifest-path Cargo.toml", false),
            ("env RUSTC_WRAPPER= cargo test --config build.rustc=evil", false),
            ("cargo test --config=evil", false),
            ("cargo test --target-dir ../target", false),
            ("rustup run stable cargo test --target-dir=/tmp/target", false),
            ("cargo test -Z unstable-options", false),
            ("cargo test -Zunstable-options", false),
            ("cargo test -vZunstable-options", false),
            ("cargo test -C ../other", false),
            ("cargo test -C../other", false),
            ("cargo test -vCother", false),
        ] {
            assert_eq!(policy.allows(&request(command, dir.path()), dir.path()), expected, "{command:?}");
        }
    }

    #[test]
    #[cfg(feature = "native")]
    fn policy_checks_real_cargo_paths() {
        use std::os::unix::fs::symlink;
        use std::process::Command;

        // Isolate PATH in a child, without mutating the environment of parallel tests.
        let Some(fixture) = std::env::var_os("ATOMICS_APPROVAL_PATH_FIXTURE") else {
            let dir = crate::test_support::tempdir().unwrap();
            for folder in ["first", "second", "configured", "linked", "worktree"] {
                std::fs::create_dir(dir.path().join(folder)).unwrap();
            }
            for folder in ["first", "second", "configured"] {
                let path = dir.path().join(folder).join("cargo");
                crate::test_support::write_program(&path, "#!/bin/sh\nexit 0\n");
            }
            symlink(dir.path().join("first/cargo"), dir.path().join("linked/cargo")).unwrap();
            symlink(dir.path().join("first/cargo"), dir.path().join("linked/rustc")).unwrap();
            let output = crate::test_support::output(
                Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "approvals::tests::policy_checks_real_cargo_paths", "--nocapture"])
                    .env("ATOMICS_APPROVAL_PATH_FIXTURE", dir.path())
                    .env("PATH", std::env::join_paths([dir.path().join("first"), dir.path().join("second")]).unwrap()),
            );
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            return;
        };
        let fixture = PathBuf::from(fixture);
        let root = fixture.join("worktree");
        let configured = format!("{} test", fixture.join("configured/cargo").display());
        let policy = ApprovalPolicy { allow: vec!["cargo test".into(), configured], ..ApprovalPolicy::default() };
        for (folder, expected) in [("first", true), ("linked", true), ("second", false), ("configured", true)] {
            let cargo = fixture.join(folder).join("cargo");
            for command in [
                format!("{} test", cargo.display()),
                format!("{} +stable test", cargo.display()),
                format!("rustup run stable {} test", cargo.display()),
                format!("atomics queue --class test -- {} test && {} test", cargo.display(), cargo.display()),
            ] {
                assert_eq!(policy.allows(&request(&command, &root), &root), expected, "{command}");
                assert!(!ApprovalPolicy::default().allows(&request(&command, &root), &root), "{command}");
            }
        }
        for command in [
            format!("{} test", fixture.join("linked/rustc").display()),
            format!("{} test --config=evil", fixture.join("first/cargo").display()),
            format!(
                "{} test && {} test",
                fixture.join("first/cargo").display(),
                fixture.join("second/cargo").display()
            ),
        ] {
            assert!(!policy.allows(&request(&command, &root), &root), "{command}");
        }
    }

    #[test]
    #[cfg(feature = "native")]
    fn relative_path_entries_never_make_an_agent_cargo_trusted() {
        use std::process::Command;

        // Isolate PATH in a child, without mutating the environment of parallel tests.
        let Some(fixture) = std::env::var_os("ATOMICS_APPROVAL_RELATIVE_PATH_FIXTURE") else {
            let dir = crate::test_support::tempdir().unwrap();
            for folder in ["host", "worktree/bin"] {
                std::fs::create_dir_all(dir.path().join(folder)).unwrap();
                let path = dir.path().join(folder).join("cargo");
                crate::test_support::write_program(&path, "#!/bin/sh\nexit 0\n");
            }
            crate::test_support::copy_program(dir.path().join("host/cargo"), dir.path().join("worktree/cargo"));
            let path = std::env::join_paths([PathBuf::new(), PathBuf::from("bin"), dir.path().join("host")]).unwrap();
            let output = crate::test_support::output(
                Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "approvals::tests::relative_path_entries_never_make_an_agent_cargo_trusted"])
                    .current_dir(dir.path().join("worktree"))
                    .env("ATOMICS_APPROVAL_RELATIVE_PATH_FIXTURE", dir.path())
                    .env("PATH", path),
            );
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            return;
        };
        let fixture = PathBuf::from(fixture);
        let root = fixture.join("worktree");
        let policy = ApprovalPolicy { allow: vec!["cargo test".into()], ..ApprovalPolicy::default() };
        let allows = |cargo: PathBuf| policy.allows(&request(&format!("{} test", cargo.display()), &root), &root);
        assert!(!allows(root.join("cargo")), "an empty PATH entry must not resolve to the agent's directory");
        assert!(!allows(root.join("bin/cargo")), "a relative PATH entry must not resolve inside the agent's directory");
        assert!(allows(fixture.join("host/cargo")), "the first absolute PATH entry is the trusted cargo");
    }

    #[test]
    #[cfg(feature = "native")]
    fn queued_commands_are_judged_by_what_they_run() {
        let dir = crate::test_support::tempdir().unwrap();
        let policy = ApprovalPolicy { allow: vec!["cargo test".into(), "ls".into()], ..ApprovalPolicy::default() };
        let allows = |command: &str| policy.allows(&request(command, dir.path()), dir.path());
        assert!(allows("atomics queue --class test -- cargo test -p atomics-core"));
        assert!(allows("atomics queue -- cargo test"));
        // Only the `atomics` ATOMICS puts first on PATH: a binary of that name elsewhere could be anything.
        assert!(!allows("./atomics queue -- ls"));
        assert!(!allows("/tmp/evil/atomics queue -- git status"));
        assert!(!allows("atomics queue -- ls\nrm -rf src"));
        assert!(!allows("atomics queue -- ls\rrm -rf src"));
        assert!(!allows("atomics queue --class test -- cargo publish"));
        assert!(!allows("atomics queue -- rm -rf target"));
        assert!(!allows("atomics queue -- cargo test; touch pwned"));
        assert!(!allows("atomics queue --class test -- ls /etc"));
        assert!(!allows("atomics queue --class bogus -- cargo test"));
    }

    #[test]
    #[cfg(feature = "native")]
    fn policy_waits_for_writes_shell_syntax_external_paths_and_execution_flags() {
        let dir = crate::test_support::tempdir().unwrap();
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
        assert!(
            !ApprovalPolicy { allow: vec![], ..ApprovalPolicy::default() }
                .allows(&request("ls", dir.path()), dir.path())
        );
        assert!(
            !ApprovalPolicy::default()
                .allows(&ApprovalRequest { tool: "mcp__x__y".into(), ..request("ls", dir.path()) }, dir.path())
        );
    }

    #[test]
    #[cfg(feature = "native")]
    fn policy_resolves_symlinks_and_custom_prefixes() {
        let root = crate::test_support::tempdir().unwrap();
        let outside = crate::test_support::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        for command in ["cat escape/secret", "ls escape", "cat escape/../secret"] {
            assert!(!ApprovalPolicy::default().allows(&request(command, root.path()), root.path()));
        }
        let policy = ApprovalPolicy { allow: vec!["echo".into()], ..ApprovalPolicy::default() };
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
    fn permission_choices_without_a_heading_or_focus_marker_reserve_input() {
        for screen in [
            "1. Yes, proceed (y)\n2. No, and tell Codex what to do differently (esc)",
            "1. Yes\n2. Yes, and don't ask again\n3. No",
            "Would you like to run the following\ncommand?\n  $ example",
        ] {
            assert!(command_prompt(screen), "{screen}");
            assert!(native_menu(screen).is_none());
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
        assert!(!menu_visible(
            "› 1. Yes, proceed\nPress enter to confirm or esc to cancel\nWorking (esc to interrupt)\n›"
        ));
    }

    #[test]
    fn claudes_folder_trust_is_a_question_whichever_answer_it_focuses() {
        let trust = include_str!("../tests/fixtures/claude-2.1.330-folder-trust.txt");
        assert_eq!(first_run_hint(trust), Some(FOLDER_ACCESS_HINT));
        assert!(first_run_prompt(trust) && answered_in_terminal(FOLDER_ACCESS_HINT));
        let yes = trust.replace("❯ No, exit\n   Yes", "  No, exit\n ❯ Yes");
        assert_eq!(first_run_hint(&yes), Some(FOLDER_ACCESS_HINT));
        assert_eq!(first_run_hint(&format!("{trust}\n\n❯ go on\n  ? for shortcuts")), None);
    }

    #[test]
    fn folder_trust_from_captured_screens_selects_only_the_named_outcome() {
        use FolderTrustDecision::{DontTrust, Trust};
        let claude = include_str!("../tests/fixtures/claude-2.1.330-folder-trust.txt");
        let codex = include_str!("../tests/fixtures/codex-0.170-folder-trust.txt");
        let restricted = include_str!("../tests/fixtures/codex-0.170-folder-restricted.txt");
        for (screen, trust, refusal, yes, no) in [
            (claude, FolderAccess::Trust, FolderAccess::Exit, &b"\x1b[B"[..], &b""[..]),
            (codex, FolderAccess::Trust, FolderAccess::Exit, &b""[..], &b"\x1b[B"[..]),
            (restricted, FolderAccess::Restricted, FolderAccess::Exit, &b""[..], &b"\x1b[B"[..]),
        ] {
            let menu = folder_trust_menu(screen).unwrap();
            assert_eq!(menu.access(Trust), trust);
            assert_eq!(menu.access(DontTrust), refusal);
            assert_eq!(menu.navigation(Trust), yes);
            assert_eq!(menu.navigation(DontTrust), no);
            assert!(folder_trust_menu(&format!("{screen}\n› Ask anything")).is_none());
        }
        let claude_yes = claude.replace("❯ No, exit\n   Yes", "  No, exit\n ❯ Yes");
        let menu = folder_trust_menu(&claude_yes).unwrap();
        assert!(menu.same(&folder_trust_menu(claude).unwrap()));
        assert_eq!(menu.navigation(DontTrust), b"\x1b[A");
        let both = codex.replace("  2. Back", "  2. Open restricted\n  3. Back");
        let menu = folder_trust_menu(&both).unwrap();
        assert_eq!(menu.access(Trust), FolderAccess::Trust);
        assert_eq!(menu.access(DontTrust), FolderAccess::Restricted);
        assert_eq!(menu.navigation(DontTrust), b"\x1b[B");
        let moved = codex.replace("› 1. Trust", "  1. Trust").replace("  2. Back", "› 2. Back");
        let menu = folder_trust_menu(&moved).unwrap();
        assert_eq!(menu.navigation(Trust), b"\x1b[A");
        assert!(!menu.same(&folder_trust_menu(restricted).unwrap()));
        assert!(folder_trust_menu(&codex.replace("  2. Back", "  3. Back")).is_none());
    }

    #[test]
    fn claudes_tip_and_rules_are_no_part_of_the_command() {
        let screen = "──────\n Bash command\n Tip: auto mode handles these prompts for you — choose \"switch to auto mode\"\n below\n Check the repo root\n╌╌╌╌╌╌╌╌╌╌\n ls -la && git status\n╌╌╌╌╌╌╌╌╌╌\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. Yes, and switch to auto mode · auto mode handles these prompts for you\n   3. No\n\n Esc to cancel · Tab to amend";
        let (body, choices) = claude_parts(screen).unwrap();
        assert_eq!(body, ["Check the repo root", "ls -la && git status"]);
        for decision in [Decision::Allow, Decision::AllowAlways] {
            assert_eq!(claude_key(&choices, decision).as_deref(), Some("\r"), "never the second choice");
        }
        let shown = "Check the repo root\nls -la && git status";
        assert_eq!(menu_key(screen, Some(shown), Decision::Allow).as_deref(), Some("\r"));
        assert_eq!(menu_key(screen, Some("ls -la && git status"), Decision::Allow), None);
        assert_eq!(menu_key(screen, Some(shown), Decision::Deny).as_deref(), Some("\x1b"));
    }

    #[test]
    fn chief_menu_status_ends_when_the_composer_returns() {
        let claude = "❯ 1. Stop and wait for limit to reset\n2. Wait here, then continue automatically at midnight\nEnter to confirm · Esc to cancel";
        let codex = "› 1. Use session directory (/repos)\n2. Use current directory (/repos/app)\nenter continue · esc use session · ⌃c quit";
        let resume = "This session is 3h 2m old and 214k tokens.\n\nResuming the full session uses a lot of your usage limits. Resuming from a summary uses less.\n\n❯ 1. Resume from summary (instant, recommended)\n  2. Resume full session as-is\n  3. Don't ask again\n\nEnter to confirm · Esc to cancel";
        let offer = "✻ Cogitated for 1s · done 18:04\n\n────\n❯\n────\n  ⏵⏵ auto mode on (shift+tab to cycle)\n\n────\n  Teach auto mode about your environment?\n\n  ❯ 1. Yes\n    2. Not now\n    3. Don't show again\n\n  Enter to confirm · Esc to cancel";
        assert_eq!(chief_menu_hint(offer), Some(QUICK_ANSWER));
        for menu in [claude, resume, codex, offer] {
            assert!(chief_menu_hint(menu).is_some_and(answered_in_terminal), "{menu}");
            assert_eq!(chief_menu_hint(&format!("{menu}\n› Ask anything")), None);
        }
    }

    #[test]
    fn a_cli_offering_to_update_itself_says_so() {
        let now = "  Update available · 0.171.0 → 0.172.0\n  Release notes: https://github.com/openai/codex/releases/latest\n\n\
                   › 1. Update now (runs `sh -c 's=$(curl -fsSL https://chatgpt.com/codex/\n     install.sh)'`)\n  2. Skip\n\
                   \x20 3. Skip until next version\n\n  enter continue · esc skip";
        let before =
            "✨ Update available! 0.165.0 -> 0.170.0\n\n› 1. Update now\n  2. Skip\n  3. Skip until next version";
        for screen in [now, before] {
            assert_eq!(first_run_hint(screen), Some(UPDATE_OFFERED), "{screen}");
            assert!(first_run_prompt(screen));
        }
        assert_eq!(first_run_hint("› Ask Codex to do anything"), None);
    }

    #[test]
    fn first_run_questions_are_prompts_and_an_idle_composer_is_not() {
        assert_eq!(first_run_hint("Sign in with ChatGPT\nPress Enter to continue"), Some("needs sign-in"));
        assert_eq!(first_run_hint("Select login method:\n❯ 1. Subscription"), Some("needs sign-in"));
        assert_eq!(first_run_hint("Choose the text style that looks best with your terminal"), Some("first-run setup"));
        assert_eq!(first_run_hint("Choose a theme"), Some("first-run setup"));
        assert_eq!(first_run_hint("Select theme:\n› 1. Dark\n  2. Light"), Some("first-run setup"));
        for screen in [
            "  Trust this folder? Codex can read, edit, and run files here, subject to your\n  permission settings.\n\n  Trust and continue\n  Open restricted",
            "> You are in /repos/app\n\n  Do you trust the files in this folder?\n\n❯ 1. Yes, proceed\n  2. No, exit",
            "✨ Update available! 0.165.0 -> 0.170.0\n\n› 1. Update now\n  2. Skip\n  3. Skip until next version",
            "  Sign in with ChatGPT to use Codex as part of your paid plan\n\n  Press Enter to continue",
        ] {
            assert!(first_run_prompt(screen), "{screen}");
        }
        for screen in ["", "› Ask Codex to do anything", "1. A numbered answer\n2. Another\n\n› Implement {feature}"]
        {
            assert!(!first_run_prompt(screen), "{screen}");
        }
    }

    #[test]
    fn codex_folder_access_explains_the_wait_and_blocks_remote_keys() {
        for (screen, folder) in [
            (include_str!("../tests/fixtures/codex-0.166-folder-restricted.txt"), Some("/home/atomics/repos")),
            (include_str!("../tests/fixtures/codex-0.170-folder-restricted.txt"), Some("/home/atomics/repos")),
            (include_str!("../tests/fixtures/codex-0.166-folder-trust.txt"), None),
            (include_str!("../tests/fixtures/codex-0.170-folder-trust.txt"), None),
        ] {
            assert_eq!(first_run_hint(screen), Some("waiting for folder access"), "{screen}");
            assert!(command_prompt(screen), "{screen}");
            assert_eq!(native_menu(screen), None, "{screen}");
            assert_eq!(codex_restricted_folder(screen).as_deref(), folder);
            let previous = format!("{screen}\n› Ask Codex to do anything");
            assert_eq!(first_run_hint(&previous), None);
            assert!(!command_prompt(&previous));
            assert_eq!(codex_restricted_folder(&previous), None);
        }
        let partial = "Folder access\n/home/atomics/repos\nConfig, hooks, and exec policies from untrusted";
        assert_eq!(first_run_hint(partial), Some(FOLDER_ACCESS_HINT));
        assert!(command_prompt(partial));
        assert_eq!(codex_restricted_folder(partial), None);
    }

    #[test]
    fn p5_review_first_run_text_is_not_a_prompt() {
        for prose in [
            "Next: review hooks in hook.rs",
            "Added a Login with GitHub button",
            "The blog in with examples is ready",
            "Users can sign in with your existing account",
            "Click sign in to continue after creating the page",
            "The choose theme action now works",
            "Review\nhooks",
            "Review hooks\n\n› Ask Codex to do anything",
        ] {
            assert_eq!(first_run_hint(prose), None, "{prose}");
            assert!(!first_run_prompt(prose), "{prose}");
        }
        let old_screen = format!("Review hooks\n{}", "ordinary output\n".repeat(20));
        assert_eq!(first_run_hint(&old_screen), None);
        assert!(!first_run_prompt(&old_screen));
        assert_eq!(first_run_hint("Select theme:\n› 1. Dark\n  2. Light\n\n"), Some("first-run setup"));
    }

    #[test]
    fn cursor_and_copilot_trust_and_mcp_menus_are_first_run_prompts() {
        let cursor_trust = "\
┌────────────────────────────────────────────────────────┐
│  ⚠ Workspace Trust Required                            │
│  /tmp/worktree                                         │
│  Do you trust the contents of this directory?          │
│  ▶ [a] Trust this workspace                            │
│    [q] Quit                                            │
│  Use arrow keys to navigate, Enter to select           │
└────────────────────────────────────────────────────────┘";
        assert_eq!(first_run_hint(cursor_trust), Some(FOLDER_ACCESS_HINT));
        assert!(first_run_prompt(cursor_trust));
        assert!(first_run_menu_visible(cursor_trust));

        let cursor_mcp = "\
│  MCP Server Approval Required                          │
│  The following MCP servers need to be approved:        │
│    plugin-atomics-launch-atomics                       │
│  ▶ [a] Approve all servers                             │
│    [c] Continue without approving                      │
│  Use arrow keys to navigate, Enter to select           │";
        assert_eq!(first_run_hint(cursor_mcp), Some("MCP approval needed"));
        assert!(first_run_prompt(cursor_mcp));
        assert!(first_run_menu_visible(cursor_mcp));

        let copilot_trust = "\
│ Confirm folder trust
│
│ /tmp/very/long/path/to/worktree
│
│ Do you trust the files in this folder?
│ Copilot may read, modify, and execute files here.
│
│ ❯ 1. Yes
│   2. Yes, and remember this folder for future sessions
│   3. No (Esc)
│
│ ↑/↓ to navigate · enter to select · esc to cancel";
        assert_eq!(first_run_hint(copilot_trust), Some(FOLDER_ACCESS_HINT));
        assert!(first_run_prompt(copilot_trust));
        assert!(first_run_menu_visible(copilot_trust));
        let answered = format!("{copilot_trust}\n❯ Ask Copilot anything");
        assert_eq!(first_run_hint(&answered), None);
        assert!(!first_run_menu_visible(&answered));
    }

    /// Claude asks "Do you trust the files in this folder?" at its first run, as it did before Copilot asked it
    /// too: Claude's is first-run setup, and Copilot's, titled and with its remember choice, folder access.
    #[test]
    fn claudes_trust_question_stays_first_run_setup() {
        let claude =
            "> You are in /repos/app\n\n  Do you trust the files in this folder?\n\n❯ 1. Yes, proceed\n  2. No, exit";
        assert_eq!(first_run_hint(claude), Some("first-run setup"));
        let copilot_tail = "│ Do you trust the files in this folder?\n│\n│ ❯ 1. Yes\n\
                            │   2. Yes, and remember this folder for future sessions\n│   3. No (Esc)";
        assert_eq!(first_run_hint(copilot_tail), Some(FOLDER_ACCESS_HINT));
    }

    /// Cursor's composer is `│ → …` in its box: once it is back under an answered trust menu, the menu is over.
    #[test]
    fn cursors_boxed_composer_ends_a_menu_above_it() {
        let trust = "\
┌────────────────────────────────────────────┐
│  ⚠ Workspace Trust Required                │
│  Do you trust the contents of this directory? │
│  ▶ [a] Trust this workspace                │
│    [q] Quit                                │
│  Use arrow keys to navigate, Enter to select │
└────────────────────────────────────────────┘";
        assert!(menu_visible(trust));
        let answered = format!(
            "{trust}\n┌──────────────────────────────┐\n│ → Plan, search, build anything │\n└──────────────────────────────┘"
        );
        assert_eq!(first_run_hint(&answered), None);
        assert!(!menu_visible(&answered));
        assert!(!first_run_menu_visible(&answered));
        // Its decision rows start with an arrow too, outside any box: they are choices, not a composer.
        assert!(menu_visible("Pick one\n› 1. First\n  → Run (once) (y)"));
    }

    const COPILOT_COMMAND_MENU: &str = "\
● Listing the files
╭──────────────────────────────────────────────────────────────────╮
│ List the files                                                   │
│ ──────────────────────────────────────────────────────────────── │
│ ╭──────────╮                                                     │
│ │ ls -la   │                                                     │
│ ╰──────────╯                                                     │
│                                                                  │
│ Do you want to run this command?                                 │
│                                                                  │
│ ❯ 1. Yes                                                         │
│   2. Yes, and approve `ls` for the rest of the running session   │
│   3. No, and tell Copilot what to do differently (Esc to stop)   │
│                                                                  │
│ ↑/↓ to navigate · enter to select · esc to cancel                │
╰──────────────────────────────────────────────────────────────────╯";
    const CURSOR_COMMAND_MENU: &str = "\
● Running cargo build
───────────────────────────────────────────────────
 $  cargo build --workspace in ~/repos/app

 Run this command?
 Not in allowlist: cargo
  → Run (once) (y)
    Add Shell(cargo) to allowlist? (tab)
    Run Everything (shift+tab)
    Skip & tell the agent what to do instead (esc or n)";

    /// Copilot's and Cursor's command menus are read whole: what they ask to run, why, and the choices that bind
    /// an answer to this very menu. Copilot is answered with Enter on its focused Yes or with Escape, Cursor with
    /// its `y` and `n`. Their trust, MCP-server and sign-in screens are not command menus.
    #[test]
    fn copilot_and_cursor_command_menus_are_native_menus_with_their_own_keys() {
        let copilot = cli_native_menu(COPILOT_COMMAND_MENU).expect("Copilot's command menu");
        assert_eq!(copilot.kind, MenuKind::Copilot);
        assert_eq!(copilot.command.as_deref(), Some("ls -la"));
        assert_eq!(copilot.reason.as_deref(), Some("List the files"));
        assert_eq!(copilot.tool, None);
        assert!(!copilot.clipped && copilot.single_command(COPILOT_COMMAND_MENU));
        assert_eq!(copilot.choices.len(), 3);
        assert_eq!(native_key(COPILOT_COMMAND_MENU, &copilot, Decision::Allow).as_deref(), Some("\r"));
        assert_eq!(native_key(COPILOT_COMMAND_MENU, &copilot, Decision::AllowAlways).as_deref(), Some("\r"));
        assert_eq!(native_key(COPILOT_COMMAND_MENU, &copilot, Decision::Deny).as_deref(), Some("\x1b"));
        // Focus moved off Yes: Enter would pick something else.
        let moved = COPILOT_COMMAND_MENU.replace("│ ❯ 1. Yes ", "│   1. Yes ").replace("│   3. No,", "│ ❯ 3. No,");
        assert_eq!(cli_native_menu(&moved).as_ref(), Some(&copilot), "the same menu");
        assert_eq!(native_key(&moved, &copilot, Decision::Allow), None);
        // Another command, or no menu at all: the keys for this one are never sent.
        let other = COPILOT_COMMAND_MENU.replace("ls -la  ", "rm -rf /");
        assert!(cli_native_menu(&other).is_some_and(|menu| !menu.same(&copilot)));
        assert_eq!(native_key(&other, &copilot, Decision::Allow), None);
        assert_eq!(native_key("● Ran ls -la\n❯ ", &copilot, Decision::Allow), None);
        assert_eq!(native_menu(COPILOT_COMMAND_MENU), None, "Claude and Codex read it as before");
        // A command that wraps is shown, but cannot be read unambiguously.
        let wrapped = COPILOT_COMMAND_MENU.replace(
            "│ │ ls -la   │                                                     │",
            "│ │ ls -la   │                                                     │\n│ │ /tmp     │                                                     │",
        );
        let wrapped = cli_native_menu(&wrapped).unwrap();
        assert_eq!(wrapped.command.as_deref(), Some("ls -la\n/tmp"));
        assert!(!wrapped.single_command(COPILOT_COMMAND_MENU));

        let cursor = cli_native_menu(CURSOR_COMMAND_MENU).expect("Cursor's command menu");
        assert_eq!(cursor.kind, MenuKind::Cursor);
        assert_eq!(cursor.command.as_deref(), Some("cargo build --workspace"));
        assert_eq!(cursor.reason.as_deref(), Some("Not in allowlist: cargo"));
        assert!(cursor.single_command(CURSOR_COMMAND_MENU));
        assert_eq!(native_key(CURSOR_COMMAND_MENU, &cursor, Decision::Allow).as_deref(), Some("y"));
        assert_eq!(native_key(CURSOR_COMMAND_MENU, &cursor, Decision::Deny).as_deref(), Some("n"));
        let mcp = "────────────\n plugin-docs: search\n   {\n     \"query\": \"parser\"\n   }\n\n Run this MCP tool?\n\
                   \x20 → Run (once) (y)\n    Allowlist MCP Tool (tab)\n    Skip (esc or n)";
        let tool = cli_native_menu(mcp).expect("Cursor's MCP tool menu");
        assert_eq!(tool.tool.as_deref(), Some("plugin-docs: search"));
        assert!(!tool.single_command(mcp), "only a shell command is answered from elsewhere");

        let cursor_trust = "│  ⚠ Workspace Trust Required │\n│  ▶ [a] Trust this workspace │\n│  Use arrow keys to navigate, Enter to select │";
        let copilot_trust = "│ Confirm folder trust\n│ Do you trust the files in this folder?\n│ ❯ 1. Yes\n│   2. Yes, and remember this folder for future sessions\n│   3. No (Esc)\n│ ↑/↓ to navigate · enter to select · esc to cancel";
        let edit = COPILOT_COMMAND_MENU.replace("Do you want to run this command?", "Do you want to edit main.rs?");
        for screen in [cursor_trust, copilot_trust, edit.as_str()] {
            assert!(cli_menu_visible(screen), "{screen}");
            assert_eq!(cli_native_menu(screen), None, "{screen}");
        }
    }

    /// Copilot and Cursor answer their own menus with what ATOMICS would type: Copilot's numbered dialogs
    /// (footer or question), Cursor's decisions and the composer that waits on one. An ordinary composer under an
    /// old question, or a transcript quoting a question, holds nothing.
    #[test]
    fn copilot_and_cursor_menus_are_read_off_their_screens() {
        let copilot_command = "\
● Listing the files
╭──────────────────────────────────────────────────────────────────╮
│ List the files                                                   │
│ ╭──────────╮                                                     │
│ │ ls -la   │                                                     │
│ ╰──────────╯                                                     │
│ Do you want to run this command?                                 │
│ ❯ 1. Yes                                                         │
│   2. Yes, and approve `ls` for the rest of the running session   │
│   3. No, and tell Copilot what to do differently (Esc to stop)   │
│ ↑/↓ to navigate · enter to select · esc to cancel                │
╰──────────────────────────────────────────────────────────────────╯";
        assert!(cli_menu_visible(copilot_command));
        assert!(!command_prompt(copilot_command), "Claude and Codex read this screen as before");
        let clipped =
            copilot_command.replace("│ ↑/↓ to navigate · enter to select · esc to cancel                │\n", "");
        assert!(cli_menu_visible(&clipped), "the question and its Yes are enough");
        let question = "│ Which package?\n│ ❯ 1. core\n│   2. ui\n│ ↑/↓ to select · enter to confirm · esc to cancel";
        assert!(cli_menu_visible(question));

        let cursor_decision = "\
● Running cargo build
───────────────────────────────────────────────────
 $ cargo build --workspace

 Run this command?
 Not in allowlist: cargo
  → Run (once) (y)
    Add Shell(cargo) to allowlist? (tab)
    Run Everything (shift+tab)
    Skip & tell the agent what to do instead (esc or n)";
        assert!(cli_menu_visible(cursor_decision));
        let mcp = cursor_decision
            .replace("Run this command?", "Run this MCP tool?")
            .replace("    Skip & tell the agent what to do instead (esc or n)", "    Skip (esc or n)");
        assert!(cli_menu_visible(&mcp));
        let waiting = "┌──────────────────────────────────────┐\n│ → Waiting for decision (y/n/p)...    │\n└──────────────────────────────────────┘";
        assert!(cli_menu_visible(waiting));

        for idle in [
            "┌──────────────────────────────┐\n│ → Add a follow-up  ctrl+c to stop │\n└──────────────────────────────┘",
            // An answered Copilot question in the transcript, under which its composer is back.
            "│ Do you want to run this command?\n│ ❯ 1. Yes\n● Ran ls -la\n❯ ",
            // A Cursor decision answered: its composer is back below it.
            &format!("{cursor_decision}\n┌──────────┐\n│ → Plan, search, build anything │\n└──────────┘"),
            "The agent explained: Run this command? → Run it once (y)",
        ] {
            assert!(!cli_menu_visible(idle), "{idle}");
        }
    }

    #[test]
    fn fallback_expands_a_hidden_command_before_approving() {
        let screen = "Would you like to run the following command?\n\nReason: A long explanation of why the command needs access\n  [… 9 lines] ⌃a view all\n\n› 1. Yes, proceed (y)\n  2. No, and tell Codex what to do differently (esc)";
        for decision in [Decision::Allow, Decision::AllowAlways, Decision::Deny] {
            assert_eq!(menu_key(screen, Some("touch fixture"), decision).as_deref(), Some("\x01"));
        }
        assert_eq!(menu_key(&screen.replace(" ⌃a view all", ""), Some("touch fixture"), Decision::Allow), None);
        assert_eq!(menu_key(&format!("{screen}\nWorking\n›"), Some("touch fixture"), Decision::Allow), None);
    }

    #[test]
    fn fallback_only_answers_visible_matching_menu_choices() {
        let screen = "Would you like to run the following command?\n\n  $ touch fixture\n\n› 1. Yes, proceed (y)\n  2. Yes, and don't ask again for commands that start with `touch`\n  3. No, and tell Codex what to do differently (esc)";
        assert_eq!(menu_key(screen, Some("touch fixture"), Decision::Allow).as_deref(), Some("1\r"));
        assert_eq!(menu_key(screen, Some("touch fixture"), Decision::AllowAlways).as_deref(), Some("1\r"));
        assert_eq!(menu_key(screen, Some("touch fixture"), Decision::Deny).as_deref(), Some("3\r"));
        assert_eq!(menu_key("Agent is working", Some("touch fixture"), Decision::Allow), None);
        assert_eq!(menu_key(screen, Some("touch other"), Decision::Allow), None);
        assert_eq!(menu_key(&format!("{screen}\nWorking\n›"), Some("touch fixture"), Decision::Allow), None);
        let old = format!("9. Yes, proceed (y)\n{screen}");
        assert_eq!(menu_key(&old, Some("touch fixture"), Decision::Allow).as_deref(), Some("1\r"));
        let no_choice =
            "1. Yes, proceed (y)\nWould you like to run the following command?\n\n$ touch fixture\n\nRendering menu";
        assert_eq!(menu_key(no_choice, Some("touch fixture"), Decision::Allow), None);
        let clipped = "Would you like to run the following command?\n\n  $ touch fixture\n\n› 1. Yes, proceed (y)\nPress enter to confirm or esc to cancel";
        assert_eq!(menu_key(clipped, Some("touch fixture"), Decision::Deny).as_deref(), Some("\x1b"));
    }

    #[test]
    fn fallback_answers_choices_listed_right_below_the_command() {
        // Codex 0.165, as seen in the e2e run: no blank line between the command and its choices.
        let screen = "  Would you like to run the following command?\n\n  Environment: local\n\n  Reason: Allow the required check to run outside the sandbox so it can\n  access the ATOMICS test queue socket?\n\n  $ atomics queue --class test -- ./check.sh\n› 1. Yes, proceed (y)\n  2. Yes, and don't ask again for commands that start with `atomics queue\n     --class test -- ./check.sh` (p)\n  Press enter to confirm or esc to cancel";
        let command = Some("atomics queue --class test -- ./check.sh");
        assert_eq!(menu_key(screen, command, Decision::Allow).as_deref(), Some("1\r"));
        assert_eq!(menu_key(screen, command, Decision::AllowAlways).as_deref(), Some("1\r"));
        assert_eq!(menu_key(screen, command, Decision::Deny).as_deref(), Some("\x1b"));
        assert_eq!(menu_key(screen, Some("atomics queue --class test -- ./other.sh"), Decision::Allow), None);
        let wrapped = screen.replace("./check.sh\n›", "./check.sh --all\n  --verbose\n›");
        assert_eq!(
            menu_key(&wrapped, Some("atomics queue --class test -- ./check.sh --all --verbose"), Decision::Allow),
            None
        );
        assert_eq!(menu_key(&wrapped, command, Decision::Allow), None);
    }

    const CODEX_SCREEN: &str = include_str!("../tests/fixtures/codex-0.165-approval-screen.txt");
    const CLAUDE_SCREEN: &str = include_str!("../tests/fixtures/claude-2.1-approval-screen.txt");

    #[test]
    fn recorded_codex_and_claude_command_menus_are_native_approvals() {
        let codex = native_menu(CODEX_SCREEN).unwrap();
        assert_eq!(codex.kind, MenuKind::Codex);
        assert_eq!(codex.command.as_deref(), Some("atomics queue --class test -- ./check.sh"));
        assert_eq!(
            codex.reason.as_deref(),
            Some("Allow the required check to run outside the sandbox so it can access the ATOMICS test queue socket?")
        );
        assert_eq!(native_key(CODEX_SCREEN, &codex, Decision::Allow).as_deref(), Some("1\r"));
        assert_eq!(native_key(CODEX_SCREEN, &codex, Decision::AllowAlways).as_deref(), Some("1\r"));
        assert_eq!(native_key(CODEX_SCREEN, &codex, Decision::Deny).as_deref(), Some("\x1b"));

        // Claude 2.1 draws the description above the command; both are what the menu shows.
        let claude = native_menu(CLAUDE_SCREEN).unwrap();
        assert_eq!(claude.kind, MenuKind::Claude);
        assert_eq!(claude.command.as_deref(), Some("Create the fixture file the test expects\ntouch fixture"));
        assert_eq!(claude.reason, None);
        assert_eq!(native_key(CLAUDE_SCREEN, &claude, Decision::Allow).as_deref(), Some("\r"));
        assert_eq!(native_key(CLAUDE_SCREEN, &claude, Decision::Deny).as_deref(), Some("\x1b"));
        // Either line could be a description. A hook's ID cannot answer this different rendering.
        for screen in [
            CLAUDE_SCREEN.to_string(),
            CLAUDE_SCREEN.replace(
                "   Create the fixture file the test expects\n   touch fixture",
                "   touch fixture\n   Create the fixture file the test expects",
            ),
        ] {
            assert_eq!(menu_key(&screen, Some("touch fixture"), Decision::Allow), None);
            assert_eq!(menu_key(&screen, Some("touch other"), Decision::Allow), None);
            assert_eq!(menu_key(&screen, Some(""), Decision::Allow), None);
        }
        assert!(menu_visible(CODEX_SCREEN) && menu_visible(CLAUDE_SCREEN));
    }

    #[test]
    fn native_keys_go_only_to_the_menu_that_was_reviewed() {
        let codex = native_menu(CODEX_SCREEN).unwrap();
        let other = CODEX_SCREEN.replace("./check.sh\n›", "./other.sh\n›");
        assert!(!native_menu(&other).unwrap().same(&codex));
        assert_eq!(native_key(&other, &codex, Decision::Allow), None);
        // A new wrap changes the text presented to the approver.
        let wrapped = CODEX_SCREEN.replace("--class test -- ./check.sh\n›", "--class test --\n    ./check.sh\n›");
        assert!(!native_menu(&wrapped).unwrap().same(&codex));
        assert_eq!(native_key(&wrapped, &codex, Decision::Allow), None);
        // Answered: the composer is back, and nothing is waiting.
        let answered =
            format!("{CODEX_SCREEN}\n• Ran atomics queue --class test -- ./check.sh\n\n› Ask Codex to do anything");
        assert_eq!(native_menu(&answered), None);
        assert_eq!(native_key(&answered, &codex, Decision::Allow), None);

        let claude = native_menu(CLAUDE_SCREEN).unwrap();
        assert_eq!(native_key(CODEX_SCREEN, &claude, Decision::Allow), None);
        assert_eq!(
            native_key(&CLAUDE_SCREEN.replace("touch fixture", "rm -rf fixture"), &claude, Decision::Allow),
            None
        );
        // Enter takes the focused choice, so Allow waits for the focus to be back on Yes.
        let moved = CLAUDE_SCREEN.replace(" ❯ 1. Yes\n   2.", "   1. Yes\n ❯ 2.");
        assert!(native_menu(&moved).unwrap().same(&claude));
        assert_eq!(native_key(&moved, &claude, Decision::Allow), None);
        assert_eq!(native_key(&moved, &claude, Decision::Deny).as_deref(), Some("\x1b"));
        let sandbox = CLAUDE_SCREEN.replace(" Bash command\n", " Bash command (unsandboxed)\n");
        assert!(native_menu(&sandbox).unwrap().same(&claude));
    }

    #[test]
    fn a_codex_command_with_a_blank_or_numbered_line_is_never_approved_in_part() {
        const SHOWN: &str = "  $ atomics queue --class test -- ./check.sh\n";
        // The recorded menu, for commands whose blank row or `1. ` line reads as the command's end.
        let blank = "python3 - <<'PY'\nimport shutil\n\nshutil.rmtree('fixture')\nPY";
        let numbered = "cat > notes.md <<'EOF'\n1. Write the fixture\nEOF\nrm -rf fixture";
        for (command, part) in [(blank, "python3 - <<'PY'\nimport shutil"), (numbered, "cat > notes.md <<'EOF'")] {
            let lines: Vec<String> = command.lines().map(|line| format!("  {line}").trim_end().to_string()).collect();
            let screen = CODEX_SCREEN.replace(SHOWN, &format!("  $ {}\n", lines.join("\n").trim_start()));
            assert!(screen.contains(command.lines().last().unwrap()), "{screen}");
            // Neither the whole command nor the part above that line becomes a request, and no key goes
            // to Codex for either: the menu stays in its terminal.
            assert_eq!(native_menu(&screen), None, "{screen}");
            for decision in [Decision::Allow, Decision::AllowAlways, Decision::Deny] {
                assert_eq!(menu_key(&screen, Some(command), decision), None, "{screen}");
                assert_eq!(menu_key(&screen, Some(part), decision), None, "{screen}");
            }
            // Where Codex offers to show it all, ATOMICS expands it rather than guess.
            let expandable = screen.replace(
                "  access the ATOMICS test queue socket?\n",
                "  access the ATOMICS test queue socket?\n  [… 2 lines] ⌃a view all\n",
            );
            let menu = native_menu(&expandable).unwrap();
            assert_eq!((menu.kind, menu.command.as_deref()), (MenuKind::Codex, None));
            assert_eq!(native_key(&expandable, &menu, Decision::Allow).as_deref(), Some("\x01"));
            assert_eq!(menu_key(&expandable, Some(part), Decision::Allow).as_deref(), Some("\x01"));
        }
        // The recorded command itself, and one that has a numbered line only as part of a longer one,
        // still read whole.
        assert_eq!(
            native_menu(CODEX_SCREEN).unwrap().command.as_deref(),
            Some("atomics queue --class test -- ./check.sh")
        );
        let inline = CODEX_SCREEN.replace("./check.sh\n›", "./check.sh 1. 2.\n›");
        assert_eq!(
            native_menu(&inline).unwrap().command.as_deref(),
            Some("atomics queue --class test -- ./check.sh 1. 2.")
        );
    }

    #[test]
    fn hidden_codex_commands_expand_and_other_menus_are_not_approvals() {
        let hidden = "Would you like to run the following command?\n\nReason: A long explanation of why the command needs access\n  [… 9 lines] ⌃a view all\n\n› 1. Yes, proceed (y)\n  2. No, and tell Codex what to do differently (esc)\nPress enter to confirm or esc to cancel";
        let menu = native_menu(hidden).unwrap();
        assert_eq!((menu.kind, menu.command.as_deref()), (MenuKind::Codex, None));
        assert_eq!(menu.reason.as_deref(), Some("A long explanation of why the command needs access"));
        for decision in [Decision::Allow, Decision::Deny] {
            assert_eq!(native_key(hidden, &menu, decision).as_deref(), Some("\x01"));
        }
        let truncated = "Would you like to run the following command?\n\n  $ python3 - <<'PY'\n  from pathlib import Path\n  [… 46 lines] ⌃a view all\n\n› 1. Yes, proceed (y)\n  2. No, and tell Codex what to do differently (esc)";
        assert_eq!(native_menu(truncated).unwrap().command, None);
        assert_eq!(native_menu(&truncated.replace(" ⌃a view all", "")), None, "nothing shows the whole command");
        for screen in [
            "Allow these edits?\n› 1. Yes, proceed\n  2. No",
            "Terminal input required\nPress enter to confirm or esc to cancel",
            " Edit file\n\n src/main.rs\n\n Do you want to make this edit to main.rs?\n ❯ 1. Yes\n   2. No (esc)",
            " Tool use\n\n   fetch(url: \"https://example.com\")\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. No (esc)",
            " Bash command finished\n\n Tool use\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. No (esc)",
            " Bash command\n\n   touch fixture\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. No (esc)\n\n> ",
            "  Do you trust the files in this folder?\n\n❯ 1. Yes, proceed\n  2. No, exit",
        ] {
            assert_eq!(native_menu(screen), None, "{screen}");
        }
    }

    #[test]
    fn fallback_expands_truncated_heredocs_and_requires_the_full_command() {
        let command = format!("python3 - <<'PY'\nfrom pathlib import Path\n{}PY", "print('fixture')\n".repeat(46));
        let screen = "Would you like to run the following command?\n\n  $ python3 - <<'PY'\n  from pathlib import Path\n  [… 46 lines] ⌃a view all\n\n› 1. Yes, proceed (y)\n  2. No, and tell Codex what to do differently (esc)\nPress enter to confirm or esc to cancel";
        for decision in [Decision::Allow, Decision::AllowAlways, Decision::Deny] {
            assert_eq!(menu_key(screen, Some(&command), decision).as_deref(), Some("\x01"));
        }
        let different = command.replace("print('fixture')", "print('different command')");
        assert_eq!(menu_key(screen, Some(&different), Decision::Allow).as_deref(), Some("\x01"));
        let expanded =
            screen.replace("python3 - <<'PY'\n  from pathlib import Path\n  [… 46 lines] ⌃a view all", &command);
        assert_eq!(menu_key(&expanded, Some(&command), Decision::Allow).as_deref(), Some("1\r"));
        assert_eq!(menu_key(&expanded, Some(&different), Decision::Allow), None);
        assert_eq!(menu_key(screen, Some("python3 - <<'PY'\nimport os\nPY"), Decision::Allow), None);
        let no_hint = screen.replace(" ⌃a view all", "");
        assert_eq!(menu_key(&no_hint, Some(&command), Decision::Allow), None);
        for marker in ["[… lines]", "[… many lines]", "[… 46 lines] unrelated text"] {
            assert_eq!(
                menu_key(&screen.replace("[… 46 lines] ⌃a view all", marker), Some(&command), Decision::Allow),
                None
            );
        }
        let no_prefix = screen.replace("python3 - <<'PY'\n  from pathlib import Path\n  ", "");
        assert_eq!(menu_key(&no_prefix, Some(&command), Decision::Allow), None);
    }

    #[test]
    #[cfg(feature = "native")]
    fn builder_nix_build_prefix_matches_plain_arguments() {
        let dir = crate::test_support::tempdir().unwrap();
        let prefix = "limactl shell atomics-builder -- nix build";
        let policy = ApprovalPolicy { allow: vec![prefix.into()], ..ApprovalPolicy::default() };
        for command in [
            prefix,
            "limactl shell atomics-builder -- nix build .",
            "limactl shell atomics-builder -- nix build .#atomics-vm-image",
            "limactl shell atomics-builder -- nix build --no-link '.#atomics-vm-image'",
            "limactl shell 'atomics-builder' -- nix build .#atomics-vm-release-bundle --print-out-paths",
            "atomics queue --class test -- limactl shell atomics-builder -- nix build .#atomics-vm-image --no-link",
        ] {
            assert!(policy.allows(&request(command, dir.path()), dir.path()), "{command}");
        }
        for command in [
            "limactl shell atomics-builder-other -- nix build .#atomics-vm-image",
            "limactl shell atomics-builder -- nix builder .#atomics-vm-image",
            "limactl shell atomics-builder -- nix run .#atomics-vm-image",
        ] {
            assert!(!policy.allows(&request(command, dir.path()), dir.path()), "{command}");
        }
    }

    #[test]
    #[cfg(feature = "native")]
    fn builder_nix_build_prefix_does_not_extend_to_shell_syntax() {
        let dir = crate::test_support::tempdir().unwrap();
        let prefix = "limactl shell atomics-builder -- nix build";
        let policy = ApprovalPolicy { allow: vec![prefix.into(), "ls".into()], ..ApprovalPolicy::default() };
        let build = format!("{prefix} .#atomics-vm-image");
        for base in [prefix, build.as_str()] {
            for suffix in [
                "; other-command",
                "&& other-command",
                "| ls",
                "$(ls)",
                "`ls`",
                "> output",
                ">> output",
                "< input",
                "2>&1",
                "&",
                "*",
                "# comment",
            ] {
                let command = format!("{base} {suffix}");
                assert!(!policy.allows(&request(&command, dir.path()), dir.path()), "{command}");
            }
        }
        for command in [
            format!("{prefix} '.#web'; ls"),
            format!("ls && {prefix} \".#web\""),
            format!("{build}; ls"),
            format!("{build} && ls"),
            format!("ls && {build}"),
            format!("{prefix}; # tail"),
            format!("{build}\nls"),
        ] {
            assert!(!policy.allows(&request(&command, dir.path()), dir.path()), "{command}");
        }
    }

    #[test]
    fn builder_prefixes_do_not_bypass_paths_flags_or_queue_unwrapping() {
        let dir = crate::test_support::tempdir().unwrap();
        let outside = crate::test_support::tempdir().unwrap();
        let file = outside.path().join("output");
        std::fs::write(&file, "output").unwrap();
        let bare = "limactl shell atomics-builder";
        let wrapped = "atomics queue --class test -- limactl shell atomics-builder";
        for allow in [vec![bare.into()], vec![bare.into(), wrapped.into()]] {
            let policy = ApprovalPolicy { allow, ..Default::default() };
            for prefix in [bare, wrapped] {
                for suffix in [format!("-- cat {}", file.display()), "-- ps -C rustc".into()] {
                    let command = format!("{prefix} {suffix}");
                    assert!(!policy.allows(&request(&command, dir.path()), dir.path()), "{command}");
                }
                assert!(policy.allows(&request(&format!("{prefix} -- cat local.txt"), dir.path()), dir.path()));
            }
        }
        let policy = ApprovalPolicy { allow: vec![wrapped.into()], ..Default::default() };
        assert!(
            !policy.allows(&request(&format!("{wrapped} -- cat local.txt"), dir.path()), dir.path()),
            "queue is unwrapped in requests, not in configured prefixes"
        );
    }
}
