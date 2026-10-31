//! MCP server definitions from Claude Code's and Codex's config files, for ATOMICS's connections:
//! each server's name, URL or command and arguments. Environment and header values are secrets:
//! a definition keeps their names (or count) only. [`env_values`] reads one server's environment
//! again, for the caller to store with the user's consent; nothing here prints a value.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where a definition was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// Claude Code's user config, `~/.claude.json`: user and project (local) servers.
    ClaudeCode,
    /// Claude Code's managed servers, `managed-mcp.json` in the system folder.
    ClaudeManaged,
    /// Codex's `config.toml`.
    Codex,
    /// Added in Settings → Connections, or asked for by an agent.
    Added,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::ClaudeCode => "Claude Code",
            Source::ClaudeManaged => "Claude Code (managed)",
            Source::Codex => "Codex",
            Source::Added => "added",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Transport {
    /// A remote server: sbx's gateway connects to it, with OAuth when it needs it.
    Url { url: String },
    /// A local stdio server, run on this machine.
    Command {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
}

/// One server, as ATOMICS registers it with sbx. Never holds a secret value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Definition {
    /// The name sbx registers it under: lowercase letters, digits, `-` and `_`.
    pub name: String,
    pub source: Source,
    /// Its key in the source file, to find its environment again.
    #[serde(default)]
    pub key: String,
    /// The project a Claude Code local server belongs to (its key under `projects`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(flatten)]
    pub transport: Transport,
    /// The names of its environment variables; their values stay in the source file until stored.
    #[serde(default)]
    pub env: Vec<String>,
    /// Header, bearer-token and pass-through settings left out: they hold or name secrets.
    #[serde(default)]
    pub skipped: usize,
}

impl Definition {
    /// What the Connections list shows: a URL without credentials, query or fragment, or the
    /// command with secret-looking arguments masked.
    pub fn display(&self) -> String {
        match &self.transport {
            Transport::Url { url } => display_url(url),
            Transport::Command { command, args, .. } => display_command(command, args),
        }
    }
}

/// Only the `mcpServers` of a Claude Code config: the user's, and each project's under
/// `projects`. Serde skips everything else in the file without keeping it.
#[derive(Deserialize)]
struct ClaudeFile {
    #[serde(default, rename = "mcpServers")]
    servers: Option<serde_json::Map<String, Value>>,
    #[serde(default)]
    projects: Option<BTreeMap<String, ClaudeProject>>,
}

#[derive(Deserialize)]
struct ClaudeProject {
    #[serde(default, rename = "mcpServers")]
    servers: Option<serde_json::Map<String, Value>>,
}

impl ClaudeFile {
    fn parse(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|_| "not valid JSON".to_string())
    }

    fn servers(&self, project: Option<&str>) -> Option<&serde_json::Map<String, Value>> {
        match project {
            Some(project) => self.projects.as_ref()?.get(project)?.servers.as_ref(),
            None => self.servers.as_ref(),
        }
    }
}

/// Only the `mcp_servers` of a Codex config.
#[derive(Deserialize)]
struct CodexFile {
    #[serde(default)]
    mcp_servers: Option<toml::Table>,
}

impl CodexFile {
    fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|_| "not valid TOML".to_string())
    }
}

/// The servers in Claude Code's `~/.claude.json` (user servers, then each project's) or a
/// `managed-mcp.json`, which share their `mcpServers` shape. Unreadable entries are skipped.
pub fn claude(text: &str, source: Source) -> Result<Vec<Definition>, String> {
    let file = ClaudeFile::parse(text)?;
    let projects = file.projects.iter().flatten().map(|(project, _)| Some(project.as_str()));
    let mut found = Vec::new();
    for project in std::iter::once(None).chain(projects) {
        for (key, server) in file.servers(project).into_iter().flatten() {
            if let Some(mut definition) = claude_server(key, server, source) {
                definition.project = project.map(str::to_string);
                found.push(definition);
            }
        }
    }
    Ok(found)
}

fn claude_server(key: &str, server: &Value, source: Source) -> Option<Definition> {
    let kind = server.get("type").and_then(Value::as_str).unwrap_or("stdio");
    let names = |field: &str| -> Vec<String> {
        server.get(field).and_then(Value::as_object).map(|map| map.keys().cloned().collect()).unwrap_or_default()
    };
    let transport = match kind {
        "http" | "sse" => Transport::Url { url: server.get("url")?.as_str()?.to_string() },
        "stdio" => Transport::Command {
            command: server.get("command")?.as_str()?.to_string(),
            args: strings(server.get("args"))?,
            cwd: None,
        },
        // IDE and SDK servers only exist inside their host process.
        _ => return None,
    };
    let skipped = names("headers").len() + usize::from(server.get("headersHelper").is_some());
    definition(key, source, transport, names("env"), skipped)
}

/// The servers in Codex's `config.toml`, without disabled ones.
pub fn codex(text: &str) -> Result<Vec<Definition>, String> {
    let file = CodexFile::parse(text)?;
    let mut found = Vec::new();
    for (key, server) in file.mcp_servers.iter().flatten() {
        let Some(server) = server.as_table() else { continue };
        if server.get("enabled").and_then(toml::Value::as_bool) == Some(false) {
            continue;
        }
        let names = |field: &str| -> Vec<String> {
            server.get(field).and_then(toml::Value::as_table).map(|t| t.keys().cloned().collect()).unwrap_or_default()
        };
        let transport = if let Some(url) = server.get("url").and_then(toml::Value::as_str) {
            Transport::Url { url: url.to_string() }
        } else if let Some(command) = server.get("command").and_then(toml::Value::as_str) {
            let args = match server.get("args") {
                None => Vec::new(),
                Some(args) => {
                    let Some(args) = args.as_array().and_then(|args| {
                        args.iter().map(|arg| arg.as_str().map(str::to_string)).collect::<Option<Vec<_>>>()
                    }) else {
                        continue;
                    };
                    args
                }
            };
            let cwd = server.get("cwd").and_then(toml::Value::as_str).map(str::to_string);
            Transport::Command { command: command.to_string(), args, cwd }
        } else {
            continue;
        };
        let skipped = names("http_headers").len()
            + names("env_http_headers").len()
            + usize::from(server.contains_key("bearer_token_env_var"))
            + server.get("env_vars").and_then(toml::Value::as_array).map_or(0, Vec::len);
        found.extend(definition(key, Source::Codex, transport, names("env"), skipped));
    }
    Ok(found)
}

fn strings(value: Option<&Value>) -> Option<Vec<String>> {
    match value {
        None | Some(Value::Null) => Some(Vec::new()),
        Some(value) => value.as_array()?.iter().map(|arg| arg.as_str().map(str::to_string)).collect(),
    }
}

fn definition(key: &str, source: Source, transport: Transport, env: Vec<String>, skipped: usize) -> Option<Definition> {
    let name = sbx_name(key)?;
    if let Transport::Command { command, args, .. } = &transport {
        // ATOMICS's own server: every sandbox already gets one of its own.
        let program = command.rsplit('/').next().unwrap_or(command);
        if program == "atomics" && args.first().is_some_and(|arg| arg == "mcp") {
            return None;
        }
    }
    if let Transport::Url { url } = &transport
        && !(url.starts_with("https://") || url.starts_with("http://"))
    {
        return None;
    }
    Some(Definition { name, source, key: key.to_string(), project: None, transport, env, skipped })
}

/// A name sbx accepts and ATOMICS's own servers (`atomics-<session>`) never use, from a config key.
pub fn sbx_name(key: &str) -> Option<String> {
    let mut name = String::new();
    for c in key.trim().chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() || c == '_' {
            name.push(c);
        } else if !name.ends_with('-') {
            name.push('-');
        }
    }
    let name = name.trim_matches('-');
    if name.is_empty() {
        return None;
    }
    let mut name: String = name.chars().take(48).collect();
    if name.starts_with("atomics-") {
        name.insert_str(0, "user-");
    }
    Some(name)
}

/// Whether `name` is one ATOMICS may register, load or remove: never ATOMICS's per-sandbox servers.
pub fn valid_name(name: &str) -> bool {
    sbx_name(name).as_deref() == Some(name)
}

/// Keeps the first definition of each name, in the order found.
pub fn dedup(definitions: Vec<Definition>) -> Vec<Definition> {
    let mut seen = std::collections::BTreeSet::new();
    definitions.into_iter().filter(|definition| seen.insert(definition.name.clone())).collect()
}

/// The environment of the server `definition` names, read again from its source file's `text`,
/// for the caller to store with the user's consent. Values are never displayed.
pub fn env_values(text: &str, definition: &Definition) -> Result<BTreeMap<String, String>, String> {
    let env = match definition.source {
        Source::ClaudeCode | Source::ClaudeManaged => {
            let file = ClaudeFile::parse(text)?;
            let servers = file.servers(definition.project.as_deref());
            servers.and_then(|servers| servers.get(&definition.key)?.get("env")).map(|env| {
                env.as_object()
                    .into_iter()
                    .flatten()
                    .filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_string())))
                    .collect::<BTreeMap<_, _>>()
            })
        }
        Source::Codex => {
            let file = CodexFile::parse(text)?;
            file.mcp_servers
                .as_ref()
                .and_then(|servers| servers.get(&definition.key)?.get("env")?.as_table().cloned())
                .map(|env| {
                    env.into_iter()
                        .filter_map(|(name, value)| Some((name, value.as_str()?.to_string())))
                        .collect::<BTreeMap<_, _>>()
                })
        }
        Source::Added => None,
    };
    let env = env.ok_or_else(|| format!("{} no longer defines {}", definition.source.label(), definition.key))?;
    Ok(env.into_iter().filter(|(name, _)| valid_env_name(name)).collect())
}

pub fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && name.len() <= 128
}

/// A URL as the Connections list shows it: scheme, host and path, without user info, query or
/// fragment, which can carry a key.
pub fn display_url(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let path = if path.is_empty() { String::new() } else { format!("/{path}") };
    if scheme.is_empty() { format!("{host}{path}") } else { format!("{scheme}://{host}{path}") }
}

/// A command line as the Connections list shows it: the value after a flag that names a secret,
/// such a flag's `=value`, and arguments shaped like a token are masked.
pub fn display_command(command: &str, args: &[String]) -> String {
    let mut shown = vec![command.to_string()];
    let mut hide_next = false;
    for arg in args {
        if std::mem::take(&mut hide_next) {
            shown.push("•••".into());
            continue;
        }
        if let Some((flag, _)) = arg.split_once('=')
            && flag.starts_with('-')
            && secret_word(flag)
        {
            shown.push(format!("{flag}=•••"));
        } else if arg.starts_with('-') && secret_word(arg) {
            shown.push(arg.clone());
            hide_next = true;
        } else if token_like(arg) {
            shown.push("•••".into());
        } else {
            shown.push(arg.clone());
        }
    }
    shown.join(" ")
}

fn secret_word(flag: &str) -> bool {
    let flag = flag.to_ascii_lowercase();
    ["token", "key", "secret", "password", "passwd", "auth", "credential", "bearer"]
        .iter()
        .any(|word| flag.contains(word))
}

/// Known token prefixes, or a long run of letters and digits without the dots and slashes of a
/// package, path or URL.
fn token_like(arg: &str) -> bool {
    const PREFIXES: [&str; 8] = ["sk-", "ghp_", "gho_", "github_pat_", "lin_api_", "xox", "glpat-", "AKIA"];
    PREFIXES.iter().any(|prefix| arg.starts_with(prefix))
        || (arg.len() >= 24
            && !arg.contains(['/', '.', '@', ':', ' '])
            && arg.chars().any(|c| c.is_ascii_digit())
            && arg.chars().any(|c| c.is_ascii_alphabetic()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: &str = r#"{
        "numStartups": 3,
        "oauthAccount": {"emailAddress": "PRIVATE-account"},
        "primaryApiKey": "sk-ant-api03-PRIVATE-key",
        "mcpServers": {
            "Linear": {"type": "http", "url": "https://mcp.linear.app/mcp"},
            "github": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"],
                       "env": {"GITHUB_PERSONAL_ACCESS_TOKEN": "ghp_PRIVATEvalue123456789"}},
            "sentry": {"type": "sse", "url": "https://user:PRIVATE-pass@mcp.sentry.dev/sse?key=PRIVATE-query",
                       "headers": {"Authorization": "Bearer PRIVATE-header"}},
            "ide": {"type": "sse-ide", "url": "http://127.0.0.1:1234/sse"},
            "atomics": {"command": "/Applications/ATOMICS.app/Contents/MacOS/atomics", "args": ["mcp"]},
            "broken": {"type": "stdio", "args": ["no command"]}
        },
        "projects": {
            "/Users/max/repos/app": {"allowedTools": [], "mcpServers": {
                "postgres": {"type": "stdio", "command": "docker", "args": ["run", "-i", "--rm", "mcp/postgres"],
                             "env": {"DATABASE_URL": "postgres://PRIVATE"}},
                "github": {"command": "other"}
            }},
            "/Users/max/repos/other": {"mcpServers": null}
        }
    }"#;

    const CODEX: &str = r#"
model = "gpt-6"
[mcp_servers.playwright]
command = "npx"
args = ["-y", "@playwright/mcp@latest", "--browser", "chrome"]

[mcp_servers.node_repl]
command = "/Applications/ChatGPT.app/node_repl"
startup_timeout_sec = 30
[mcp_servers.node_repl.env]
NODE_REPL_NODE_PATH = "PRIVATE-node-path"
"bad name" = "PRIVATE-bad"

[mcp_servers."Computer Use"]
command = "./Codex Computer Use.app/client"
args = ["mcp"]
cwd = "/Applications"
enabled = false

[mcp_servers.notion]
url = "https://mcp.notion.com/mcp"
bearer_token_env_var = "NOTION_TOKEN"
http_headers = { "X-Workspace" = "PRIVATE-header" }
env_vars = ["NOTION_ORG"]
"#;

    fn names(definitions: &[Definition]) -> Vec<&str> {
        definitions.iter().map(|d| d.name.as_str()).collect()
    }

    #[test]
    fn claude_code_servers_keep_names_urls_commands_and_env_names_only() {
        let found = claude(CLAUDE, Source::ClaudeCode).unwrap();
        assert_eq!(names(&found), ["linear", "github", "sentry", "github", "postgres"]);
        let linear = &found[0];
        assert_eq!(linear.key, "Linear");
        assert_eq!(linear.transport, Transport::Url { url: "https://mcp.linear.app/mcp".into() });
        let github = &found[1];
        assert_eq!(github.env, ["GITHUB_PERSONAL_ACCESS_TOKEN"]);
        assert_eq!(found[2].skipped, 1, "the Authorization header is left out");
        assert_eq!(found[4].project.as_deref(), Some("/Users/max/repos/app"));
        let debug = format!("{found:?} {}", serde_json::to_string(&found).unwrap());
        for secret in ["ghp_PRIVATE", "PRIVATE-header", "postgres://PRIVATE", "PRIVATE-account", "sk-ant-api03"] {
            assert!(!debug.contains(secret), "{secret} leaked into {debug}");
        }
        let unique = dedup(found);
        assert_eq!(names(&unique), ["linear", "github", "sentry", "postgres"]);
        assert_eq!(
            unique[1].transport,
            Transport::Command {
                command: "npx".into(),
                args: vec!["-y".into(), "@modelcontextprotocol/server-github".into()],
                cwd: None
            }
        );
        assert!(claude("not json", Source::ClaudeCode).is_err());
        assert!(claude("{}", Source::ClaudeManaged).unwrap().is_empty());
    }

    #[test]
    fn codex_servers_skip_disabled_ones_and_count_secret_settings() {
        let found = codex(CODEX).unwrap();
        assert_eq!(names(&found), ["node_repl", "notion", "playwright"]);
        assert_eq!(found[0].env, ["NODE_REPL_NODE_PATH", "bad name"]);
        assert_eq!(found[1].transport, Transport::Url { url: "https://mcp.notion.com/mcp".into() });
        assert_eq!(found[1].skipped, 3, "bearer variable, header and passed-through variable");
        assert!(!format!("{found:?}").contains("PRIVATE"));
        assert!(codex("[mcp_servers").is_err());
        assert!(codex("model = 'x'").unwrap().is_empty());
    }

    #[test]
    fn env_values_come_back_only_for_the_named_server_and_valid_names() {
        let claude_servers = claude(CLAUDE, Source::ClaudeCode).unwrap();
        let postgres = claude_servers.iter().find(|d| d.name == "postgres").unwrap();
        assert_eq!(
            env_values(CLAUDE, postgres).unwrap(),
            [("DATABASE_URL".into(), "postgres://PRIVATE".into())].into()
        );
        let github = &claude_servers[1];
        assert_eq!(env_values(CLAUDE, github).unwrap().len(), 1);
        let node = codex(CODEX).unwrap().remove(0);
        assert_eq!(
            env_values(CODEX, &node).unwrap(),
            [("NODE_REPL_NODE_PATH".into(), "PRIVATE-node-path".into())].into()
        );
        let error = env_values("{}", github).unwrap_err();
        assert!(error.contains("no longer defines github") && !error.contains("PRIVATE"), "{error}");
    }

    #[test]
    fn names_are_safe_for_sbx_and_never_atomics_session_servers() {
        assert_eq!(sbx_name("Computer Use").as_deref(), Some("computer-use"));
        assert_eq!(sbx_name("claude.ai Linear").as_deref(), Some("claude-ai-linear"));
        assert_eq!(sbx_name("atomics-abc").as_deref(), Some("user-atomics-abc"));
        assert_eq!(sbx_name("  --  "), None);
        assert!(valid_name("linear") && valid_name("node_repl"));
        assert!(!valid_name("atomics-123") && !valid_name("Linear") && !valid_name("a b") && !valid_name(""));
    }

    #[test]
    fn displays_mask_credentials_in_urls_and_arguments() {
        assert_eq!(display_url("https://user:PRIVATE@mcp.sentry.dev/sse?key=PRIVATE#x"), "https://mcp.sentry.dev/sse");
        assert_eq!(display_url("https://mcp.linear.app/mcp"), "https://mcp.linear.app/mcp");
        assert_eq!(display_url("https://example.com?token=PRIVATE"), "https://example.com");
        let args = [
            "-y",
            "@playwright/mcp@latest",
            "--api-key",
            "PRIVATE-one",
            "--token=PRIVATE-two",
            "ghp_PRIVATEthree",
            "abcdefghijklmnopqrstuvwxyz0123456789",
            "/srv/data",
        ]
        .map(String::from);
        let shown = display_command("npx", &args);
        assert_eq!(shown, "npx -y @playwright/mcp@latest --api-key ••• --token=••• ••• ••• /srv/data");
    }
}
