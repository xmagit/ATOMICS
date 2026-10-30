#[cfg(feature = "native")]
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::layout::LayoutConfig;

/// Written to `~/.config/imacs/config.toml` on first start. Every key is optional;
/// anything left out falls back to the defaults below.
pub const DEFAULT_CONFIG: &str = r##"# IMACS configuration. Saved changes are picked up while IMACS is running.
# config.d/<hostname>.toml overrides individual keys (IMACS_HOST overrides hostname).
# Use ~/ paths in shared config; they expand on the machine running IMACS.

# Every diff is taken against the merge-base of this ref and the worktree's HEAD,
# so commits landing on main never show up as your agent's changes.
base_ref = "origin/main"

# Repositories whose worktrees are listed in the sidebar, even without an agent.
repos = []

# Where "New worktree" puts new worktrees. {repo} is the repository folder name,
# {slug} the branch name with "/" replaced by "-". Siblings of the repo by default.
worktree_path = "{repo_parent}/{repo}-{slug}"

[cleanup]
completed_plans = true        # remove clean task worktrees and merged task branches
archived_plans = true         # remove clean task worktrees; keep branches

[worktrees]
# Clone existing cache folders from the main checkout before starting an agent.
# Copy-on-write only (APFS on macOS, reflink on Linux); unsupported clones are skipped.
# Paths are relative to the repository root. [] turns warming off.
warm = ["target"]

[layout]
min_tile_width = 520.0      # below this a terminal tile collapses into a tab
min_tile_height = 280.0
sidebar_width = 250.0
sidebar_min_window = 1100.0 # hide the sidebar in narrower windows
diff_min_width = 560.0
diff_fraction = 0.55        # share of the main area the diff takes when side by side
diff_max_width = 1400.0     # beyond this, extra width goes to agent columns (0 = no cap)
diff_min_height = 260.0
diff_split = "auto"         # old and new side by side: "auto" (diff area 1100+ wide), "always", "never"

[font]
size = 13.0
diff_size = 12.5

[terminal]
scrollback = 5000           # lines each terminal keeps; a full 20000 on a wide terminal takes about 80 MB

[theme]
name = "claude"             # built in: claude, midnight, atomisk, paper and tide (light); or themes/<name>.toml beside this file
# Any theme token set here wins over the theme; docs/themes.md lists them all. For example:
# accent = "#d77757"
# syntax = "Monokai Extended"   # any bat/two-face theme
# mono_ui = true                 # monospace for all UI text
# ui_font = "Inter"              # an installed family or a font file
# mono_size = 13.0               # terminal text, over [font] size

[isolation]
# Where agents run. "sandbox" gives each agent its own Docker Sandboxes microVM (sbx) that
# sees only its worktree and git's write folders; "bare" runs agents directly on this machine.
# An agent below, or a plan task, can set its own isolation = "bare" or "sandbox"; a repository's
# .imacs/config.toml can require "sandbox" and select its template. See docs/isolation.md.
mode = "sandbox"
# bare_relay = true          # bare Claude sessions use a keychain-backed relay placeholder too

[isolation.sandbox]
# memory = "8g"              # per sandbox; unset = 8g; "" = sbx's default, half of this machine's memory
# cpus = 4                   # unset = [resources] jobs ÷ test_slots, at least 2; 0 = every core
# template = "registry.example.com/agent-sandbox:1"   # container image; unset = the agent's own
mounts = []                  # extra host folders, mounted read-only
network = "balanced"         # sbx's global policy, set up if it has none: "balanced", "allow-all", "deny-all"
ssh_agent = false            # let sandboxes sign with this machine's SSH agent
mcp = []                     # MCP servers registered with `sbx mcp add`, loaded into every sandbox
# Hosts a repository's sandboxes may reach beyond the network policy, by its main checkout:
# allow_hosts = { "~/repos/app" = ["nuget.example.com"] }

# Agents you can start. kind = "claude" turns on hooks, session ids and resume.
[[agents]]
name = "claude"
command = ["claude"]
kind = "claude"

[[agents]]
name = "codex"
command = ["codex"]
kind = "generic"
busy_text = "esc to interrupt"   # screen text that means "working", for agents without hooks

[[agents]]
name = "shell"
command = []                # empty = your login shell
kind = "shell"

[approvals]
# Command prefixes within the session worktree; every part of an && or ; chain must pass.
# Cargo toolchains and trusted absolute cargo paths use the same cargo prefix rules.
# [] makes every request wait, including commands with env prefixes or queue wrappers.
allow = ["ls", "cat", "rg", "git status", "git diff", "git log"]
# Exact NAME=value assignments for shell prefixes and env (no options).
# For example ["RUSTC_WRAPPER="] allows only that empty value, not a wrapper program.
env = []

[workflow]
# IMACS completion milestone; later stages come from the chief or stage_event webhooks.
done = "accepted"
# stages = ["accepted", "integrated", "pr_opened", "ci_passed", "merged", "deployed"]

[plan]
# Close accepted, integrated agents automatically. Worktrees remain available.
close_done_agents = true

# Ticket workflow profiles. Choose ONE block, or copy it into a workspace file.
# A workspace's [tickets] replaces this section. See docs/tickets.md for setup and scoping.
# Tools and result paths are config only; use doctor to check tool names and sync for paths.
# BEGIN RELAY PROFILE
# Relay: one existing board per plan (tool names are placeholders; adjust every map).
# [tickets]
# system = "relay"
# mode = "board"
# comments = "agents"
# workflow = "Link the existing Relay board to the plan and one ticket to each task. Product writes what and why; the chief comments implementation details. IMACS posts signed agent reports and maps states live. Use imacs_report for reports, without a second ticket comment."
# server = { command = ["relay-mcp"] }
# states = { planned = "Todo", running = "In Progress" }
#
# [tickets.operations.get]
# tool = "get_ticket"
# arguments = { ticket = "{id}" }
# result = { id = "ticket.id", key = "ticket.key", url = "ticket.url", title = "ticket.title", state = "ticket.status", updated_at = "ticket.updated_at" }
#
# [tickets.operations.list]
# tool = "list_tickets"
# arguments = { board = "{board}" }
# items = "tickets"
# result = { id = "id", key = "key", url = "url", title = "title", state = "status", updated_at = "updated_at" }
#
# [tickets.operations.update_state]
# tool = "update_ticket"
# arguments = { ticket = "{id}", status = "{state}" }
#
# [tickets.operations.comment]
# tool = "add_comment"
# arguments = { ticket = "{id}", body = "{body}" }
#
# [tickets.operations.create]
# tool = "create_ticket"
# arguments = { board = "{board}", title = "{title}", description = "{body}" }
# result = { id = "ticket.id", key = "ticket.key", url = "ticket.url", title = "ticket.title", state = "ticket.status" }
# END RELAY PROFILE
#
# BEGIN LINEAR PROFILE
# Linear: one issue per plan (authenticate the bridge in a terminal first).
# [tickets]
# system = "linear"
# mode = "ticket"
# comments = "chief"
# workflow = "One Linear issue per plan; the issue says what and why, the plan says how."
# server = { command = ["npx", "-y", "mcp-remote", "https://mcp.linear.app/mcp"] }
# states = { planned = "Todo", running = "In Progress" }
#
# [tickets.operations.get]
# tool = "get_issue"
# arguments = { id = "{key}" }
# result = { id = "", key = "id", url = "url", title = "title", state = "status", updated_at = "updatedAt", branch = "gitBranchName" }
#
# [tickets.operations.update_state]
# tool = "save_issue"
# arguments = { id = "{key}", state = "{state}" }
#
# [tickets.operations.comment]
# tool = "save_comment"
# arguments = { issueId = "{key}", body = "{body}" }
#
# [tickets.operations.create]
# tool = "save_issue"
# arguments = { team = "ENG", title = "{title}", description = "{body}" }
# result = { id = "id", key = "id", url = "url", title = "title", state = "status" }
# END LINEAR PROFILE

[resources]
# Heavy work shared by every agent IMACS starts. Compilers share one jobserver pool, and whole
# test runs or QA instances wait for a slot through `imacs queue --class test|qa -- <command>`,
# as do the chief's checks in agents' sandboxes (imacs_exec).
# jobs = 16          # jobserver tokens; unset = cores minus 2; 0 = no pool (build scripts that run Apple make)
# test_slots = 3     # whole test runs at once; unset = one per 6 cores
# qa_slots = 1       # QA instances at once
agent_nice = 10      # niceness agent terminals start at; 0 = normal priority

[resources.dotnet]
# Applied automatically when the worktree contains a .sln, .slnx, .csproj or global.json.
enabled = true
# processor_count = 2  # per-process CPUs; unset = resources.jobs / test_slots / 2, at least 1

[editor]
# Terminal editor for editor mode (cmd+e). Helix has LSP and inlay hints built in.
command = ["hx"]
# Keystrokes that open a file at a line in the running editor; each step is sent
# after a lone Escape. Neovim: [":e {path}\r", ":{line}\r"]
open_keys = [":open {path}\r", ":goto {line}\r"]
# Neovim: [":lua vim.lsp.inlay_hint.enable(not vim.lsp.inlay_hint.is_enabled())\r"]
inlay_hints_keys = [":toggle lsp.display-inlay-hints\r"]
# Typed once when an editor starts. base16_transparent uses IMACS's own colors. [] to keep yours.
startup_keys = [":theme base16_transparent\r"]

[chief]
# The chief agent (cmd+o), floating or docked beside the agents.
heartbeat_secs = 300         # fleet checks, delivered through imacs_wait
dock = "floating"   # "floating" or "docked"
command = ["claude"]
args = ["--model", "claude-opus-5-5"]
# system_prompt = "..."   # built in: plan, wait for approval, dispatch, verify, integrate
# prompt = "..."          # your own instructions, added after the built-in prompt when the chief starts;
                          # config.d/<host>.toml and the workspace file add theirs after this one

[steer]
# Added to every Claude session started or seen by IMACS (SessionStart hook).
session_context = "This session runs inside IMACS. The user reviews this worktree's diff against {base_ref} live and may send review comments or steering messages while you work. Messages that arrive mid-turn are delivered as hook context and come directly from the user."
review_header = "Review comments on your changes (diff against {base_ref}):"
# Messages to sessions running outside IMACS wait for that session's next tool call or turn end.
external_prefix = "Message from the user, sent from IMACS while you were working:"

[voice]
# Push-to-talk (cmd+shift+space): press, speak, press again to send the transcript. Esc cancels.
provider = "openai"         # "openai": any OpenAI-compatible /audio/transcriptions (OpenAI, Groq, a local Whisper server)
                            # "openrouter": an audio-capable chat model on openrouter.ai
target = "chief"            # "chief", or "active" for the active agent
# Left out, these follow the provider:
# base_url = "https://api.openai.com/v1"   # Groq: "https://api.groq.com/openai/v1"
# model = "gpt-4o-mini-transcribe"         # openrouter: "google/gemini-2.5-flash"
# api_key_env = "OPENAI_API_KEY"           # openrouter: "OPENROUTER_API_KEY"; also read from your login shell
# api_key = "..."                          # discouraged: keep the key in the environment variable instead

[keys]
toggle_diff = "cmd+d"
next_attention = "cmd+j"
focus_steer = "cmd+l"
new_session = "cmd+t"
refresh_diff = "cmd+r"
close_session = "cmd+w"
quick_open = "cmd+p"
toggle_editor = "cmd+e"
toggle_plan = "cmd+b"
toggle_chief = "cmd+o"
voice = "cmd+shift+space"
"##;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub base_ref: String,
    pub repos: Vec<String>,
    pub worktree_path: String,
    pub worktrees: WorktreeConfig,
    pub cleanup: CleanupConfig,
    pub layout: LayoutConfig,
    pub font: FontConfig,
    pub terminal: TerminalConfig,
    pub theme: ThemeConfig,
    pub isolation: crate::isolation::IsolationConfig,
    pub agents: Vec<AgentConfig>,
    pub approvals: crate::approvals::ApprovalPolicy,
    pub plan: PlanConfig,
    pub workflow: crate::workflow::Workflow,
    pub resources: crate::governor::ResourceConfig,
    pub editor: EditorConfig,
    pub chief: ChiefConfig,
    pub steer: SteerConfig,
    pub voice: VoiceConfig,
    pub keys: KeyConfig,
    pub tickets: crate::tickets::TicketConfig,
}

/// Automatic cleanup on plan completion and archival. Dirty worktrees are always kept.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CleanupConfig {
    pub completed_plans: bool,
    pub archived_plans: bool,
}

impl Default for CleanupConfig {
    fn default() -> Self {
        Self { completed_plans: true, archived_plans: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WorktreeConfig {
    pub warm: Vec<String>,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self { warm: vec!["target".into()] }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PlanConfig {
    pub close_done_agents: bool,
}

impl Default for PlanConfig {
    fn default() -> Self {
        Self { close_done_agents: true }
    }
}

/// How the chief agent works, given to it as an appended system prompt.
/// `{projects}` and `{base_ref}` are filled in.
pub const CHIEF_PROMPT: &str = "You are the chief agent of IMACS: you orchestrate coding agents for the user, who watches \
every agent, diff and the plan board in the same window. Open projects: {projects}. Diffs are taken against {base_ref}.

How to work:
1. Understand the goal and read the code you need. You run in the project's main checkout: read it, never edit it.
2. Write the plan with imacs_plan_set: small tasks, each with a brief (context, files, constraints), acceptance checks you \
can verify, dependencies, the agent (claude for design-heavy work, codex for well-specified changes) and its args. \
Without a plan id, imacs_plan_set creates a separate plan. Keep its returned id and pass plan to every plan tool. \
Ask the user to approve it on its own plan board. Never dispatch before approval.
3. imacs_plan_dispatch, then loop on imacs_wait (never sleep or poll). On reported or turn done, read imacs_result and imacs_diff.
Include heartbeat in imacs_wait statuses. On a heartbeat with no findings, post one short line; otherwise act on the findings.
To restart IMACS, rebuild/install first, then call imacs_restart; wait instead of forcing while agents work.
When an event contains approval, use imacs_approve with its session id to answer that request, regardless of the selected plan tab.
4. Verify every acceptance check yourself, and read the diff with imacs_diff. A sandboxed session's code (imacs_list \
shows its sandbox) runs only in its VM: run its builds and tests with imacs_exec, which runs them in its sandbox, and \
never run builds, tests or git in its worktree on this machine. Run a bare session's checks in its worktree. When \
something is missing, steer with imacs_comment + imacs_send_review or imacs_send and move the task back to running. Accept \
with imacs_plan_update (status done, a note saying what you verified).
5. Integrate accepted work locally. Every plan ends with a review task: a fresh agent reviews all of the plan's work, \
including yours, and in sandbox mode runs its checks in its own sandbox. Send its blocking findings back to the tasks \
they concern, and only then ask the user to open the pull requests. Never push, open pull requests or delete branches without the user's OK.

Several plans may be open at once, each with its own tab, agents, approval, integration branch and final review. \
Start the next plan while another finishes. The chief is never closed. Use imacs_plan_get without plan for summaries.
Close finished plans with imacs_plan_close (plan id, status completed or archived); their history remains available by id.
To configure IMACS itself, read imacs_config_get (and imacs_doctor when something fails) and propose changes with \
imacs_config_propose: the user applies or rejects each one, and the outcome arrives as a config event. Never edit \
IMACS's config files yourself.
Agents share this machine: their compilers draw from one job pool, and full test runs and QA instances wait for a slot through `imacs queue`, as imacs_exec does. Size the fleet to the machine, and keep briefs asking for targeted checks while working and one full test run at the end.

Keep briefs, notes and messages short: the user reads them on the board.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChiefDock {
    #[default]
    Floating,
    Docked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ChiefConfig {
    pub heartbeat_secs: u64,
    pub dock: ChiefDock,
    pub command: Vec<String>,
    pub args: Vec<String>,
    pub system_prompt: String,
    /// The user's instructions, added after the built-in prompt. Every file's adds to the
    /// others' (main, host, then the workspace's), so read them with
    /// [`crate::selfconfig::prompt_sections`]: merged, this holds only the last file's.
    pub prompt: String,
}

impl Default for ChiefConfig {
    fn default() -> Self {
        Self {
            heartbeat_secs: 300,
            dock: ChiefDock::Floating,
            command: vec!["claude".into()],
            args: vec!["--model".into(), "claude-opus-5-5".into()],
            system_prompt: CHIEF_PROMPT.into(),
            prompt: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorConfig {
    pub command: Vec<String>,
    pub open_keys: Vec<String>,
    pub inlay_hints_keys: Vec<String>,
    pub startup_keys: Vec<String>,
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            command: vec!["hx".into()],
            open_keys: vec![":open {path}\r".into(), ":goto {line}\r".into()],
            inlay_hints_keys: vec![":toggle lsp.display-inlay-hints\r".into()],
            startup_keys: vec![":theme base16_transparent\r".into()],
        }
    }
}

impl EditorConfig {
    pub fn open_steps(&self, path: &str, line: Option<u32>, fresh: bool) -> Vec<String> {
        let mut steps = vec!["\x1b".into()];
        if fresh {
            steps.extend(self.startup_keys.clone());
        }
        steps.extend(
            self.open_keys
                .iter()
                .filter(|step| (line.is_some() || !step.contains("{line}")) && (!fresh || step.contains("{line}")))
                .map(|step| step.replace("{path}", path).replace("{line}", &line.unwrap_or(1).to_string())),
        );
        steps
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FontConfig {
    pub size: f32,
    pub diff_size: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalConfig {
    pub scrollback: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    /// A built-in theme or `themes/<name>.toml` beside the config; `preset` is the old key.
    #[serde(alias = "preset")]
    pub name: String,
    /// Tokens set here win over the named theme.
    #[serde(flatten)]
    pub tokens: crate::theme::ThemeTokens,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Claude,
    Generic,
    Shell,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub name: String,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default = "AgentConfig::default_kind")]
    pub kind: AgentKind,
    /// Text the agent shows while it works. Used to track status of agents without hooks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub busy_text: Option<String>,
    /// Runs this agent sandboxed or bare, whatever `[isolation] mode` says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<crate::isolation::IsolationMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SteerConfig {
    pub session_context: String,
    pub review_header: String,
    pub external_prefix: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoiceProvider {
    /// `POST {base_url}/audio/transcriptions`: OpenAI, Groq, local Whisper servers.
    #[default]
    Openai,
    /// An audio-capable chat model behind OpenRouter's chat completions.
    Openrouter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoiceTarget {
    #[default]
    Chief,
    Active,
}

/// Push-to-talk transcription. Unset fields fall back to the provider's defaults.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceConfig {
    pub provider: VoiceProvider,
    pub target: VoiceTarget,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Never serialized or printed.
    #[serde(skip_serializing)]
    pub api_key: Option<String>,
}

impl std::fmt::Debug for VoiceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceConfig")
            .field("provider", &self.provider)
            .field("target", &self.target)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key_env", &self.api_key_env)
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .finish()
    }
}

impl VoiceConfig {
    pub fn base_url(&self) -> &str {
        let default = match self.provider {
            VoiceProvider::Openai => "https://api.openai.com/v1",
            VoiceProvider::Openrouter => "https://openrouter.ai/api/v1",
        };
        self.base_url.as_deref().filter(|u| !u.trim().is_empty()).unwrap_or(default).trim().trim_end_matches('/')
    }

    pub fn model(&self) -> &str {
        let default = match self.provider {
            VoiceProvider::Openai => "gpt-4o-mini-transcribe",
            VoiceProvider::Openrouter => "google/gemini-2.5-flash",
        };
        self.model.as_deref().map(str::trim).filter(|m| !m.is_empty()).unwrap_or(default)
    }

    pub fn api_key_env(&self) -> &str {
        let default = match self.provider {
            VoiceProvider::Openai => "OPENAI_API_KEY",
            VoiceProvider::Openrouter => "OPENROUTER_API_KEY",
        };
        self.api_key_env.as_deref().map(str::trim).filter(|v| !v.is_empty()).unwrap_or(default)
    }

    /// `api_key` from the config, else the environment variable as `env` looks it up.
    pub fn api_key(&self, env: impl Fn(&str) -> Option<String>) -> Result<String> {
        let key = match self.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
            Some(key) => Some(key.to_string()),
            None => env(self.api_key_env()).map(|k| k.trim().to_string()).filter(|k| !k.is_empty()),
        };
        key.with_context(|| format!("{} is not set (see [voice] in the config)", self.api_key_env()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyConfig {
    pub toggle_diff: String,
    pub next_attention: String,
    pub focus_steer: String,
    pub new_session: String,
    pub refresh_diff: String,
    pub close_session: String,
    pub quick_open: String,
    pub toggle_editor: String,
    pub toggle_plan: String,
    pub toggle_chief: String,
    pub voice: String,
}

/// A parsed shortcut such as `cmd+shift+d`. `cmd` means Command on macOS and Ctrl+Shift elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    pub cmd: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub key: String,
}

impl Default for Config {
    fn default() -> Self {
        let agent = |name: &str, command: &[&str], kind, busy: Option<&str>| AgentConfig {
            name: name.into(),
            command: command.iter().map(|c| c.to_string()).collect(),
            kind,
            busy_text: busy.map(str::to_string),
            isolation: None,
        };
        Self {
            base_ref: "origin/main".into(),
            repos: Vec::new(),
            worktree_path: "{repo_parent}/{repo}-{slug}".into(),
            worktrees: WorktreeConfig::default(),
            cleanup: CleanupConfig::default(),
            layout: LayoutConfig::default(),
            font: FontConfig::default(),
            terminal: TerminalConfig::default(),
            theme: ThemeConfig::default(),
            isolation: crate::isolation::IsolationConfig::default(),
            agents: vec![
                agent("claude", &["claude"], AgentKind::Claude, None),
                agent("codex", &["codex"], AgentKind::Generic, Some("esc to interrupt")),
                agent("shell", &[], AgentKind::Shell, None),
            ],
            approvals: crate::approvals::ApprovalPolicy::default(),
            plan: PlanConfig::default(),
            workflow: crate::workflow::Workflow::default(),
            resources: crate::governor::ResourceConfig::default(),
            editor: EditorConfig::default(),
            chief: ChiefConfig::default(),
            steer: SteerConfig::default(),
            voice: VoiceConfig::default(),
            keys: KeyConfig::default(),
            tickets: crate::tickets::TicketConfig::default(),
        }
    }
}

impl Default for FontConfig {
    fn default() -> Self {
        Self { size: 13.0, diff_size: 12.5 }
    }
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self { scrollback: 5000 }
    }
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self { name: crate::theme::DEFAULT.into(), tokens: crate::theme::ThemeTokens::default() }
    }
}

impl Default for SteerConfig {
    fn default() -> Self {
        Self {
            session_context: "This session runs inside IMACS. The user reviews this worktree's diff against {base_ref} live and may send review comments or steering messages while you work. Messages that arrive mid-turn are delivered as hook context and come directly from the user.".into(),
            review_header: "Review comments on your changes (diff against {base_ref}):".into(),
            external_prefix: "Message from the user, sent from IMACS while you were working:".into(),
        }
    }
}

impl Default for KeyConfig {
    fn default() -> Self {
        Self {
            toggle_diff: "cmd+d".into(),
            next_attention: "cmd+j".into(),
            focus_steer: "cmd+l".into(),
            new_session: "cmd+t".into(),
            refresh_diff: "cmd+r".into(),
            close_session: "cmd+w".into(),
            quick_open: "cmd+p".into(),
            toggle_editor: "cmd+e".into(),
            toggle_plan: "cmd+b".into(),
            toggle_chief: "cmd+o".into(),
            voice: "cmd+shift+space".into(),
        }
    }
}

impl AgentConfig {
    pub fn is_codex(&self) -> bool {
        self.kind == AgentKind::Generic
            && self
                .command
                .first()
                .is_some_and(|program| std::path::Path::new(program).file_name().is_some_and(|name| name == "codex"))
    }

    fn default_kind() -> AgentKind {
        AgentKind::Generic
    }
}

impl Config {
    /// Loads the config, writing the commented default first if none exists.
    #[cfg(feature = "native")]
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, DEFAULT_CONFIG)?;
        }
        Self::load(path)
    }

    #[cfg(feature = "native")]
    pub fn load(path: &Path) -> Result<Self> {
        Self::load_with_sources(path).map(|(config, _)| config)
    }

    /// Base and selected host file, including a host file that does not exist yet.
    #[cfg(feature = "native")]
    pub fn source_paths(path: &Path) -> [PathBuf; 2] {
        let host_file = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("config.d")
            .join(format!("{}.toml", crate::paths::host_name()));
        [path.to_path_buf(), host_file]
    }

    /// Loads base then host overrides, recursively merging tables before applying defaults.
    /// Arrays and scalar values are replaced; only files actually read are returned.
    #[cfg(feature = "native")]
    pub fn load_with_sources(path: &Path) -> Result<(Self, Vec<PathBuf>)> {
        let mut tables = Vec::new();
        let mut sources = Vec::new();
        for (index, file) in Self::source_paths(path).into_iter().enumerate() {
            let text = match std::fs::read_to_string(&file) {
                Ok(text) => text,
                Err(err) if index > 0 && err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err).with_context(|| format!("reading {}", file.display())),
            };
            tables.push(toml::from_str(&text).with_context(|| format!("parsing {}", file.display()))?);
            sources.push(file);
        }
        let config = Self::from_tables(tables).with_context(|| {
            format!(
                "parsing merged config from {}",
                sources.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
            )
        })?;
        Ok((config, sources))
    }

    /// The config the files' tables make, base first: tables merge key by key, and arrays
    /// and other values replace what came before.
    pub fn from_tables(tables: impl IntoIterator<Item = toml::Table>) -> Result<Self, toml::de::Error> {
        let mut config: Config = toml::Value::Table(Self::merged(tables)).try_into()?;
        if config.agents.is_empty() {
            config.agents = Config::default().agents;
        }
        Ok(config)
    }

    /// The files' tables merged, base first, before defaults apply.
    pub fn merged(tables: impl IntoIterator<Item = toml::Table>) -> toml::Table {
        let mut merged = toml::Table::new();
        for table in tables {
            merge_tables(&mut merged, table);
        }
        merged
    }

    pub fn agent(&self, name: &str) -> Option<&AgentConfig> {
        self.agents.iter().find(|a| a.name == name)
    }

    /// Every agent runs sandboxed, whatever its own or its task's setting: what a repository may require.
    pub fn require_sandbox(&mut self) {
        self.isolation.mode = crate::isolation::IsolationMode::Sandbox;
        self.isolation.required = true;
    }

    /// Replaces `{base_ref}` in a configured text.
    pub fn fill(&self, template: &str) -> String {
        template.replace("{base_ref}", &self.base_ref)
    }
}

fn merge_tables(base: &mut toml::Table, overrides: toml::Table) {
    for (key, value) in overrides {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(existing)), toml::Value::Table(value)) => merge_tables(existing, value),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

impl Chord {
    pub fn parse(text: &str) -> Option<Chord> {
        let mut chord = Chord { cmd: false, ctrl: false, alt: false, shift: false, key: String::new() };
        for part in text.split('+').map(str::trim).filter(|p| !p.is_empty()) {
            match part.to_ascii_lowercase().as_str() {
                "cmd" | "command" | "super" => chord.cmd = true,
                "ctrl" | "control" => chord.ctrl = true,
                "alt" | "option" | "opt" => chord.alt = true,
                "shift" => chord.shift = true,
                _ => chord.key = part.to_string(),
            }
        }
        (!chord.key.is_empty()).then_some(chord)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::DiffSplit;

    #[cfg(feature = "native")]
    #[test]
    fn automatic_plan_cleanup_defaults_on_and_can_be_disabled_independently() {
        let defaults: Config = toml::from_str("").unwrap();
        assert!(defaults.cleanup.completed_plans && defaults.cleanup.archived_plans);
        let custom: Config = toml::from_str("[cleanup]\ncompleted_plans = false\narchived_plans = false").unwrap();
        assert!(!custom.cleanup.completed_plans && !custom.cleanup.archived_plans);
        let config: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        assert!(config.cleanup.completed_plans && config.cleanup.archived_plans);
    }

    #[test]
    fn host_config_merges_keys_and_expands_paths_on_each_machine() {
        use crate::{git, paths};
        const CHILD: &str = "IMACS_HOST_CONFIG_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            for host in [Some("work-mac"), None] {
                let dir = crate::test_support::tempdir().unwrap();
                let mut command = std::process::Command::new(std::env::current_exe().unwrap());
                command
                    .args(["--exact", "config::tests::host_config_merges_keys_and_expands_paths_on_each_machine"])
                    .env(CHILD, "1")
                    .env("HOME", dir.path())
                    .env_remove("IMACS_CONFIG")
                    .env_remove("XDG_CONFIG_HOME");
                match host {
                    Some(host) => {
                        command.env("IMACS_HOST", host);
                    }
                    None => {
                        command.env_remove("IMACS_HOST");
                    }
                }
                let output = crate::test_support::output(&mut command);
                assert!(
                    output.status.success(),
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        }
        let path = paths::config_file();
        let [_, host_file] = Config::source_paths(&path);
        if let Ok(host) = std::env::var("IMACS_HOST") {
            assert_eq!(paths::host_name(), host);
            assert_eq!(host_file.file_name().unwrap(), "work-mac.toml");
        }
        let defaults = Config::load_or_create(&path).unwrap();
        assert_eq!(defaults.worktree_path, "{repo_parent}/{repo}-{slug}");
        assert!(!std::fs::read_to_string(&path).unwrap().contains(&paths::home().display().to_string()));
        std::fs::write(
            &path,
            r#"
base_ref = "main"
repos = ["~/repos/base"]
worktree_path = "~/worktrees/{repo}-{slug}"
[layout]
sidebar_width = 300.0
diff_min_width = 600.0
[resources]
jobs = 4
test_slots = 2
[resources.dotnet]
enabled = false
processor_count = 3
[voice]
api_key = "secret-from-base"
"#,
        )
        .unwrap();
        std::fs::create_dir_all(host_file.parent().unwrap()).unwrap();
        // Only the selected host is read, even when other host files are broken.
        std::fs::write(host_file.with_file_name("unselected host.toml"), "[invalid").unwrap();
        let (base, sources) = Config::load_with_sources(&path).unwrap();
        assert_eq!(sources, std::slice::from_ref(&path));
        assert_eq!(base.layout.sidebar_width, 300.0);
        std::fs::write(
            &host_file,
            r#"
base_ref = "work/main"
repos = ["~/repos/host", "~"]
[layout]
sidebar_width = 350.0
[resources.dotnet]
enabled = true
[worktrees]
warm = []
[[agents]]
name = "local-shell"
kind = "shell"
"#,
        )
        .unwrap();
        let (config, sources) = Config::load_with_sources(&path).unwrap();
        assert_eq!(sources, [path.clone(), host_file.clone()]);
        assert_eq!(config.base_ref, "work/main");
        assert_eq!(config.layout.sidebar_width, 350.0);
        assert_eq!(config.layout.diff_min_width, 600.0);
        assert_eq!(config.font.size, Config::default().font.size);
        assert_eq!(config.resources.jobs, Some(4));
        assert_eq!(config.resources.test_slots, Some(2));
        assert!(config.resources.dotnet.enabled);
        assert_eq!(config.resources.dotnet.processor_count, Some(3));
        assert_eq!(config.voice.api_key.as_deref(), Some("secret-from-base"));
        assert!(config.worktrees.warm.is_empty());
        assert_eq!(config.agents.len(), 1);
        assert_eq!(config.agents[0].name, "local-shell");
        assert_eq!(config.repos, ["~/repos/host", "~"]);
        assert_eq!(paths::expand(&config.repos[0]), paths::home().join("repos/host"));
        assert_eq!(paths::expand(&config.repos[1]), paths::home());
        assert_eq!(
            git::worktree_path(&config.worktree_path, &paths::expand("~/repos/app"), "fix/thing"),
            paths::home().join("worktrees/app-fix-thing")
        );

        std::fs::write(&host_file, "repos = []").unwrap();
        assert!(Config::load(&path).unwrap().repos.is_empty());
        for invalid in ["[invalid", "[layout]\nsidebar_width = 'wide'"] {
            std::fs::write(&host_file, invalid).unwrap();
            let error = format!("{:#}", Config::load(&path).unwrap_err());
            assert!(error.contains(&host_file.display().to_string()), "{error}");
        }
        std::fs::remove_file(&host_file).unwrap();
        assert_eq!(Config::load_with_sources(&path).unwrap().1, std::slice::from_ref(&path));
        std::fs::remove_file(&path).unwrap();
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn codex_detection_uses_the_program_and_agent_kind() {
        for (name, command, kind, expected) in [
            ("custom", vec!["codex"], AgentKind::Generic, true),
            ("custom", vec!["/tools/codex"], AgentKind::Generic, true),
            ("codex", vec!["other"], AgentKind::Generic, false),
            ("codex", vec![], AgentKind::Generic, false),
            ("codex", vec!["codex"], AgentKind::Shell, false),
            ("codex", vec!["codex"], AgentKind::Claude, false),
        ] {
            let agent = AgentConfig {
                name: name.into(),
                command: command.into_iter().map(str::to_string).collect(),
                kind,
                busy_text: None,
                isolation: None,
            };
            assert_eq!(agent.is_codex(), expected, "{agent:?}");
        }
    }

    #[test]
    fn approval_env_assignments_are_explicit_and_preserve_empty_values() {
        let old: Config = toml::from_str("[approvals]\nallow = ['cargo test']").unwrap();
        assert!(old.approvals.env.is_empty());
        let config: Config =
            toml::from_str("[approvals]\nenv = ['RUSTC_WRAPPER=', 'CARGO_TERM_COLOR=always']").unwrap();
        assert_eq!(config.approvals.env, ["RUSTC_WRAPPER=", "CARGO_TERM_COLOR=always"]);
        let saved = toml::to_string(&config).unwrap();
        let loaded: Config = toml::from_str(&saved).unwrap();
        assert_eq!(loaded.approvals.env, config.approvals.env);
    }

    #[test]
    fn worktree_warming_defaults_off_switch_and_custom_folders() {
        for text in ["", "[worktrees]"] {
            assert_eq!(toml::from_str::<Config>(text).unwrap().worktrees.warm, ["target"]);
        }
        assert!(toml::from_str::<Config>("[worktrees]\nwarm = []").unwrap().worktrees.warm.is_empty());
        let config: Config = toml::from_str("[worktrees]\nwarm = [\"target\", \"build/cache\"]").unwrap();
        assert_eq!(config.worktrees.warm, ["target", "build/cache"]);
        let saved = toml::to_string(&config).unwrap();
        assert_eq!(toml::from_str::<Config>(&saved).unwrap().worktrees.warm, config.worktrees.warm);
        assert!(toml::from_str::<Config>("[worktrees]\nwarm = \"target\"").is_err());
    }

    #[test]
    fn automatic_agent_closing_defaults_on_and_can_be_disabled() {
        let old: Config = toml::from_str("").unwrap();
        assert!(old.plan.close_done_agents);
        let disabled: Config = toml::from_str("[plan]\nclose_done_agents = false").unwrap();
        assert!(!disabled.plan.close_done_agents);
    }

    #[test]
    fn editor_open_steps_escape_insert_mode_before_open_and_fresh_line_jump() {
        let config = EditorConfig::default();
        assert_eq!(config.open_steps("README.md", Some(42), false), ["\x1b", ":open README.md\r", ":goto 42\r"]);
        assert_eq!(config.open_steps("README.md", None, false), ["\x1b", ":open README.md\r"]);
        assert_eq!(
            config.open_steps("README.md", Some(42), true),
            ["\x1b", ":theme base16_transparent\r", ":goto 42\r"]
        );
        assert_eq!(config.open_steps("README.md", None, true), ["\x1b", ":theme base16_transparent\r"]);
    }

    #[test]
    fn ticket_profile_examples_match_the_documentation() {
        let docs = include_str!("../../../docs/tickets.md");
        for name in ["RELAY", "LINEAR"] {
            let block = DEFAULT_CONFIG
                .split(&format!("# BEGIN {name} PROFILE\n"))
                .nth(1)
                .unwrap()
                .split(&format!("# END {name} PROFILE"))
                .next()
                .unwrap();
            let text =
                block.lines().skip(1).map(|line| line.strip_prefix("# ").unwrap_or("")).collect::<Vec<_>>().join("\n");
            let config: Config = toml::from_str(&text).unwrap();
            assert!(config.tickets.is_configured());
            assert!(docs.contains(text.trim()), "{name} example drifted from docs/tickets.md");
        }
    }

    #[test]
    fn default_config_file_matches_built_in_defaults() {
        let parsed: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        let built_in = Config::default();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), serde_json::to_value(&built_in).unwrap());
        assert_eq!(parsed.agent("claude").unwrap().kind, AgentKind::Claude);
        assert!(parsed.steer.session_context.contains("{base_ref}"));
    }

    #[test]
    fn partial_config_keeps_defaults() {
        let config: Config = toml::from_str("base_ref = \"main\"\n[layout]\nmin_tile_width = 400.0").unwrap();
        assert_eq!(config.base_ref, "main");
        assert_eq!(config.layout.min_tile_width, 400.0);
        assert_eq!(config.layout.sidebar_width, LayoutConfig::default().sidebar_width);
        assert_eq!(config.font.size, 13.0);
        assert_eq!(config.terminal.scrollback, 5000);
    }

    #[test]
    fn heartbeat_interval_defaults_and_parses() {
        assert_eq!(Config::default().chief.heartbeat_secs, 300);
        let config: Config = toml::from_str("[chief]\nheartbeat_secs = 7").unwrap();
        assert_eq!(config.chief.heartbeat_secs, 7);
        assert!(CHIEF_PROMPT.contains("On a heartbeat with no findings, post one short line"));
        assert!(CHIEF_PROMPT.contains("run its builds and tests with imacs_exec"));
    }

    #[test]
    fn chief_dock_defaults_and_parses() {
        assert_eq!(Config::default().chief.dock, ChiefDock::Floating);
        let docked: Config = toml::from_str(
            r#"[chief]
dock = "docked""#,
        )
        .unwrap();
        assert_eq!(docked.chief.dock, ChiefDock::Docked);
        assert!(
            toml::from_str::<Config>(
                r#"[chief]
dock = "other""#
            )
            .is_err()
        );
    }

    #[test]
    fn diff_split_parses() {
        let config: Config = toml::from_str("[layout]\ndiff_split = \"never\"").unwrap();
        assert_eq!(config.layout.diff_split, DiffSplit::Never);
        assert!(toml::from_str::<Config>("[layout]\ndiff_split = \"sometimes\"").is_err());
    }

    #[test]
    fn voice_defaults_follow_the_provider() {
        let config: Config = toml::from_str("[voice]\nprovider = \"openrouter\"\ntarget = \"active\"").unwrap();
        assert_eq!(config.voice.target, VoiceTarget::Active);
        assert_eq!(config.voice.base_url(), "https://openrouter.ai/api/v1");
        assert_eq!(config.voice.model(), "google/gemini-2.5-flash");
        assert_eq!(config.voice.api_key_env(), "OPENROUTER_API_KEY");
        let groq: Config = toml::from_str(
            "[voice]\nbase_url = \"https://api.groq.com/openai/v1/\"\nmodel = \"whisper-large-v3\"\napi_key_env = \"GROQ_API_KEY\"",
        )
        .unwrap();
        assert_eq!(groq.voice.provider, VoiceProvider::Openai);
        assert_eq!(groq.voice.base_url(), "https://api.groq.com/openai/v1");
        assert_eq!(groq.voice.model(), "whisper-large-v3");
        assert!(toml::from_str::<Config>("[voice]\nprovider = \"whisper\"").is_err());
    }

    #[test]
    fn voice_api_key_lookup_and_redaction() {
        let mut voice = VoiceConfig::default();
        let env = |name: &str| (name == "OPENAI_API_KEY").then(|| " sk-from-env \n".to_string());
        assert_eq!(voice.api_key(env).unwrap(), "sk-from-env");
        let err = voice.api_key(|_| Some("  ".into())).unwrap_err().to_string();
        assert!(err.contains("OPENAI_API_KEY is not set"), "{err}");
        voice.api_key = Some("sk-in-config".into());
        assert_eq!(voice.api_key(|_| None).unwrap(), "sk-in-config");
        assert!(!format!("{voice:?}").contains("sk-in-config"));
        assert!(!serde_json::to_string(&voice).unwrap().contains("sk-in-config"));
    }

    #[test]
    fn chord_parsing() {
        let chord = Chord::parse("cmd+shift+D").unwrap();
        assert!(chord.cmd && chord.shift && !chord.alt);
        assert_eq!(chord.key, "D");
        assert!(Chord::parse("cmd+").is_none());
    }
}
