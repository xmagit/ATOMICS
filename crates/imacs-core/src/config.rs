use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::layout::LayoutConfig;

/// Written to `~/.config/imacs/config.toml` on first start. Every key is optional;
/// anything left out falls back to the defaults below.
pub const DEFAULT_CONFIG: &str = r##"# IMACS configuration. Saved changes are picked up while IMACS is running.

# Every diff is taken against the merge-base of this ref and the worktree's HEAD,
# so commits landing on main never show up as your agent's changes.
base_ref = "origin/main"

# Repositories whose worktrees are listed in the sidebar, even without an agent.
repos = []

# Where "New worktree" puts new worktrees. {repo} is the repository folder name,
# {slug} the branch name with "/" replaced by "-". Siblings of the repo by default.
worktree_path = "{repo_parent}/{repo}-{slug}"

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
preset = "claude"           # "claude": near-black with Claude orange · "midnight": blue-gray
# syntax = "Monokai Extended"   # any bat/two-face theme; each preset picks one
# mono_ui = true                 # monospace for all UI text (claude preset: on)
# Any preset color can be overridden:
# background = "#0b0b0c"
# panel = "#111113"
# raised = "#18181b"
# border = "#262629"
# foreground = "#e8e4dc"
# muted = "#8c8781"
# accent = "#d77757"
# added = "#0f2e18"
# removed = "#3a1219"
# added_emphasis = "#225c2b"
# removed_emphasis = "#7a2936"

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
# Simple command prefixes, within the session worktree. [] makes every request wait.
allow = ["cargo build", "cargo test", "cargo check", "cargo clippy", "cargo fmt", "ls", "cat", "rg", "git status", "git diff", "git log"]

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
dock = "floating"   # "floating" or "docked"
command = ["claude"]
args = ["--model", "claude-opus-5-5"]
# system_prompt = "..."   # built in: plan, wait for approval, dispatch, verify, integrate

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
    pub layout: LayoutConfig,
    pub font: FontConfig,
    pub terminal: TerminalConfig,
    pub theme: ThemeConfig,
    pub agents: Vec<AgentConfig>,
    pub approvals: crate::approvals::ApprovalPolicy,
    pub editor: EditorConfig,
    pub chief: ChiefConfig,
    pub steer: SteerConfig,
    pub voice: VoiceConfig,
    pub keys: KeyConfig,
}

/// How the chief agent works, given to it as an appended system prompt.
/// `{projects}` and `{base_ref}` are filled in.
pub const CHIEF_PROMPT: &str = "You are the chief agent of IMACS: you orchestrate coding agents for the user, who watches \
every agent, diff and the plan board in the same window. Open projects: {projects}. Diffs are taken against {base_ref}.

How to work:
1. Understand the goal and read the code you need. You run in the project's main checkout: read it, never edit it.
2. Write the plan with imacs_plan_set: small tasks, each with a brief (context, files, constraints), acceptance checks you \
can verify, dependencies, the agent (claude for design-heavy work, codex for well-specified changes) and its args. \
Ask the user to approve it on the plan board. Never dispatch before approval.
3. imacs_plan_dispatch, then loop on imacs_wait (never sleep or poll). On reported or turn done, read imacs_result and imacs_diff.
4. Verify every acceptance check yourself: run the build and tests in the task's worktree and read the diff. When \
something is missing, steer with imacs_comment + imacs_send_review or imacs_send and move the task back to running. Accept \
with imacs_plan_update (status done, a note saying what you verified).
5. Integrate accepted work locally. Every plan ends with a review task: a fresh agent reviews all of the plan's work, \
including yours. Send its blocking findings back to the tasks they concern, and only then ask the user to open the pull \
requests. Never push, open pull requests or delete branches without the user's OK.

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
    pub dock: ChiefDock,
    pub command: Vec<String>,
    pub args: Vec<String>,
    pub system_prompt: String,
}

impl Default for ChiefConfig {
    fn default() -> Self {
        Self {
            dock: ChiefDock::Floating,
            command: vec!["claude".into()],
            args: vec!["--model".into(), "claude-opus-5-5".into()],
            system_prompt: CHIEF_PROMPT.into(),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    pub preset: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub syntax: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mono_ui: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub panel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raised: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub border: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub foreground: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub muted: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_emphasis: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed_emphasis: Option<String>,
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

/// A parsed shortcut such as `cmd+shift+d`. `cmd` means Command on macOS and Ctrl elsewhere.
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
        };
        Self {
            base_ref: "origin/main".into(),
            repos: Vec::new(),
            worktree_path: "{repo_parent}/{repo}-{slug}".into(),
            layout: LayoutConfig::default(),
            font: FontConfig::default(),
            terminal: TerminalConfig::default(),
            theme: ThemeConfig::default(),
            agents: vec![
                agent("claude", &["claude"], AgentKind::Claude, None),
                agent("codex", &["codex"], AgentKind::Generic, Some("esc to interrupt")),
                agent("shell", &[], AgentKind::Shell, None),
            ],
            approvals: crate::approvals::ApprovalPolicy::default(),
            editor: EditorConfig::default(),
            chief: ChiefConfig::default(),
            steer: SteerConfig::default(),
            voice: VoiceConfig::default(),
            keys: KeyConfig::default(),
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
        Self {
            preset: "claude".into(),
            syntax: None,
            mono_ui: None,
            background: None,
            panel: None,
            raised: None,
            border: None,
            foreground: None,
            muted: None,
            accent: None,
            added: None,
            removed: None,
            added_emphasis: None,
            removed_emphasis: None,
        }
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
    fn default_kind() -> AgentKind {
        AgentKind::Generic
    }
}

impl Config {
    /// Loads the config, writing the commented default first if none exists.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, DEFAULT_CONFIG)?;
        }
        Self::load(path)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut config: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if config.agents.is_empty() {
            config.agents = Config::default().agents;
        }
        Ok(config)
    }

    pub fn agent(&self, name: &str) -> Option<&AgentConfig> {
        self.agents.iter().find(|a| a.name == name)
    }

    /// Replaces `{base_ref}` in a configured text.
    pub fn fill(&self, template: &str) -> String {
        template.replace("{base_ref}", &self.base_ref)
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
