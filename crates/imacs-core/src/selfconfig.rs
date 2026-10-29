//! Self-configuration: the chief reads the effective configuration with the source of
//! each value. Nothing an agent reads here carries a secret: values of secret-looking
//! environment names, flags and arguments are masked, and every string goes through the
//! credential cleaner built from the secrets the configuration runs with.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::Config;
use crate::tickets::{Cleaner, secret_name};

/// What a masked value reads as.
pub const MASKED: &str = "***";

/// Where a configuration value comes from, lowest precedence first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Built into IMACS.
    Default,
    /// `config.toml`.
    Main,
    /// `config.d/<host>.toml`, merged over the main file key by key.
    Host,
    /// The active workspace's file. Its `[tickets]` replaces the main config's section as a whole.
    Workspace,
    /// A repository's own settings. Repositories carry none yet; their layer goes after the workspace.
    Repo,
}

/// One configuration file as read.
#[derive(Debug, Clone)]
pub struct Layer {
    pub source: Source,
    /// The file, as shown to the user.
    pub path: String,
    /// The file's text; `None` when it does not exist.
    pub text: Option<String>,
    /// The parsed file; empty when it is missing or does not parse.
    pub table: toml::Table,
    /// Why the file could not be read or parsed, without quoting its content.
    pub error: Option<String>,
}

impl Layer {
    pub fn new(source: Source, path: impl Into<String>, text: Option<String>) -> Layer {
        let parsed = text.as_deref().map(|text| toml::from_str::<toml::Table>(text).map_err(|e| parse_error(text, &e)));
        let (table, error) = match parsed {
            Some(Ok(table)) => (table, None),
            Some(Err(error)) => (toml::Table::new(), Some(error)),
            None => (toml::Table::new(), None),
        };
        Layer { source, path: path.into(), text, table, error }
    }

    /// A file that exists but could not be read.
    pub fn unreadable(source: Source, path: impl Into<String>, error: String) -> Layer {
        Layer { source, path: path.into(), text: None, table: toml::Table::new(), error: Some(error) }
    }
}

/// A TOML parse error by line and column, without the source line the error's display
/// quotes: the file may hold a secret on that line.
pub fn parse_error(text: &str, error: &toml::de::Error) -> String {
    match error.span() {
        Some(span) => {
            let before = &text[..span.start.min(text.len())];
            let line = before.matches('\n').count() + 1;
            let column = before.rsplit('\n').next().map_or(0, |line| line.chars().count()) + 1;
            format!("line {line}, column {column}: {}", error.message().trim())
        }
        None => error.message().trim().to_string(),
    }
}

/// The secrets a configuration runs with: ticket server credentials, the voice key and
/// secret-looking environment assignments, from the effective config and every file.
pub fn secrets(effective: &Config, layers: &[Layer], mut env: impl FnMut(&str) -> Option<String>) -> Vec<String> {
    let mut secrets = crate::tickets::secrets_of(&effective.tickets, &mut env);
    secrets.extend(effective.voice.api_key.clone());
    secrets.extend(env(effective.voice.api_key_env()));
    secrets.extend(effective.approvals.env.iter().filter_map(|entry| secret_assignment(entry).map(str::to_string)));
    for layer in layers {
        let table = |path: &[&str]| lookup(&layer.table, path).and_then(toml::Value::as_table);
        if let Some(env) = table(&["tickets", "server", "env"]) {
            let values = env.iter().filter(|(name, _)| secret_name(name)).filter_map(|(_, v)| v.as_str());
            secrets.extend(values.map(str::to_string));
        }
        secrets.extend(lookup(&layer.table, &["voice", "api_key"]).and_then(toml::Value::as_str).map(str::to_string));
    }
    secrets
}

/// The value of a `NAME=value` assignment whose name looks secret.
fn secret_assignment(entry: &str) -> Option<&str> {
    entry.split_once('=').filter(|(name, value)| secret_name(name) && !value.is_empty()).map(|(_, value)| value)
}

/// The effective configuration as `key → {value, source}`, every value cleaned of secrets.
/// `prefix` keeps only the keys under it, such as `tickets` or `layout.sidebar_width`.
pub fn values(effective: &Config, layers: &[Layer], cleaner: &Cleaner, prefix: Option<&str>) -> Map<String, Value> {
    let tree = toml::Table::try_from(effective).unwrap_or_default();
    let mut leaves = Vec::new();
    collect_leaves(&tree, &mut Vec::new(), &mut leaves);
    // The voice key is never serialized; whether it is set in a file is still worth knowing.
    if effective.voice.api_key.is_some() {
        leaves.push((vec!["voice".into(), "api_key".into()], toml::Value::String(MASKED.into())));
    }
    let prefix = prefix.map(str::trim).filter(|p| !p.is_empty());
    leaves
        .into_iter()
        .map(|(path, value)| (key(&path), path, value))
        .filter(|(key, ..)| prefix.is_none_or(|p| key == p || key.starts_with(&format!("{p}."))))
        .map(|(key, path, value)| {
            let value = cleaner.setting(masked(&path, serde_json::to_value(value).unwrap_or_default()));
            let mut entry = Map::new();
            entry.insert("value".into(), value);
            entry.insert("source".into(), serde_json::to_value(source_of(layers, &path)).unwrap_or_default());
            (key, Value::Object(entry))
        })
        .collect()
}

/// The source of the value at `path`: the last file that sets it, else the built-in default.
pub fn source_of(layers: &[Layer], path: &[String]) -> Source {
    let workspace_tickets =
        layers.iter().rev().find(|layer| layer.source == Source::Workspace && layer.table.contains_key("tickets"));
    if path.first().is_some_and(|first| first == "tickets")
        && let Some(workspace) = workspace_tickets
    {
        // The workspace's section replaces the main config's: what it leaves out is the default.
        return if defines(&workspace.table, path) { Source::Workspace } else { Source::Default };
    }
    layers
        .iter()
        .rev()
        .filter(|layer| matches!(layer.source, Source::Main | Source::Host))
        .find(|layer| defines(&layer.table, path))
        .map_or(Source::Default, |layer| layer.source)
}

/// Whether a file sets the value at `path`.
pub fn defines(table: &toml::Table, path: &[String]) -> bool {
    let path: Vec<&str> = path.iter().map(String::as_str).collect();
    lookup(table, &path).is_some()
}

fn lookup<'a>(table: &'a toml::Table, path: &[&str]) -> Option<&'a toml::Value> {
    let (first, rest) = path.split_first()?;
    let value = table.get(*first)?;
    match (rest.is_empty(), value) {
        (true, value) => Some(value),
        (false, toml::Value::Table(inner)) => lookup(inner, rest),
        (false, _) => None,
    }
}

/// Every value below `table`, keyed by its path. Tables are walked; arrays are values,
/// because a file that sets an array replaces it whole.
pub fn collect_leaves(table: &toml::Table, path: &mut Vec<String>, out: &mut Vec<(Vec<String>, toml::Value)>) {
    for (name, value) in table {
        path.push(name.clone());
        match value {
            toml::Value::Table(inner) => collect_leaves(inner, path, out),
            value => out.push((path.clone(), value.clone())),
        }
        path.pop();
    }
}

/// A dotted key as TOML writes it, quoting parts that are not bare keys.
pub fn key(path: &[String]) -> String {
    path.iter()
        .map(|part| {
            let bare = !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'));
            if bare { part.clone() } else { serde_json::to_string(part).unwrap_or_default() }
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// The value with what looks secret by name masked: ticket server environment values,
/// operation arguments, `NAME=value` assignments and the values of secret-looking flags.
fn masked(path: &[String], value: Value) -> Value {
    let path: Vec<&str> = path.iter().map(String::as_str).collect();
    let named_secret = match path.as_slice() {
        ["tickets", "server", "env", name] => secret_name(name),
        ["tickets", "operations", _, "arguments", .., name] => secret_name(name) && !placeholder(&value),
        _ => false,
    };
    if named_secret {
        return Value::String(MASKED.into());
    }
    match value {
        Value::Array(items) => Value::Array(masked_arguments(items)),
        other => other,
    }
}

/// Whether an operation argument is only a placeholder such as `{key}`, which IMACS fills in.
fn placeholder(value: &Value) -> bool {
    value.as_str().is_some_and(|text| {
        let text = text.trim();
        text.starts_with('{') && text.ends_with('}') && !text[1..text.len() - 1].contains(['{', '}'])
    })
}

/// Command arguments and assignments with secret values masked: `--token=…`, the argument
/// after `--api-key`, and `NAME=…` with a secret-looking name.
fn masked_arguments(items: Vec<Value>) -> Vec<Value> {
    let mut out = Vec::with_capacity(items.len());
    let mut mask_next = false;
    for item in items {
        let Value::String(text) = item else {
            mask_next = false;
            out.push(masked_value(item));
            continue;
        };
        if std::mem::take(&mut mask_next) {
            out.push(Value::String(MASKED.into()));
            continue;
        }
        let flag = text.strip_prefix('-').map(|rest| rest.trim_start_matches('-'));
        let masked = match flag.map(|flag| flag.split_once('=')) {
            Some(Some((name, value))) if secret_name(name) && !value.is_empty() => {
                format!("{}={MASKED}", &text[..text.len() - value.len() - 1])
            }
            Some(None) if flag.is_some_and(secret_name) => {
                mask_next = true;
                text
            }
            None if secret_assignment(&text).is_some() => {
                let name = text.split_once('=').map_or("", |(name, _)| name);
                format!("{name}={MASKED}")
            }
            _ => text,
        };
        out.push(Value::String(masked));
    }
    out
}

/// Arrays of tables, such as `[[agents]]`, with their command arrays masked too.
fn masked_value(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(name, item)| {
                    let item = match item {
                        Value::Array(items) => Value::Array(masked_arguments(items)),
                        other => masked_value(other),
                    };
                    (name, item)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(masked_arguments(items)),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(source: Source, text: &str) -> Layer {
        Layer::new(source, format!("{source:?}.toml"), Some(text.into()))
    }

    #[test]
    fn sources_follow_merge_order_and_workspace_tickets_replace_the_section() {
        let main = layer(
            Source::Main,
            "base_ref = 'main'\n[layout]\nsidebar_width = 300.0\n[tickets]\nsystem = 'relay'\ntimeout_secs = 9",
        );
        let host = layer(Source::Host, "[layout]\nsidebar_width = 320.0\n[[agents]]\nname = 'local'");
        let path = |key: &str| key.split('.').map(str::to_string).collect::<Vec<_>>();
        let layers = [main.clone(), host.clone()];
        assert_eq!(source_of(&layers, &path("base_ref")), Source::Main);
        assert_eq!(source_of(&layers, &path("layout.sidebar_width")), Source::Host);
        assert_eq!(source_of(&layers, &path("layout.diff_min_width")), Source::Default);
        assert_eq!(source_of(&layers, &path("agents")), Source::Host);
        assert_eq!(source_of(&layers, &path("tickets.timeout_secs")), Source::Main);
        let workspace = layer(Source::Workspace, "projects = []\n[tickets]\nsystem = 'linear'\n[layout]\nfont = 1");
        let layers = [main, host, workspace];
        assert_eq!(source_of(&layers, &path("tickets.system")), Source::Workspace);
        assert_eq!(source_of(&layers, &path("tickets.timeout_secs")), Source::Default);
        // A workspace sets nothing outside its sections.
        assert_eq!(source_of(&layers, &path("layout.sidebar_width")), Source::Host);
        assert_eq!(
            key(&["tickets".into(), "server".into(), "env".into(), "MY.VAR".into()]),
            "tickets.server.env.\"MY.VAR\""
        );
    }

    #[test]
    fn values_carry_sources_and_never_secrets() {
        let main = layer(
            Source::Main,
            r#"
base_ref = "main"
[[agents]]
name = "claude"
command = ["claude", "--api-key", "sk-agent-flag-123", "--token=tok-inline-456", "--model", "opus"]
kind = "claude"
[approvals]
env = ["GITHUB_TOKEN=ghp-assignment-789", "CARGO_TERM_COLOR=always"]
[voice]
api_key = "sk-voice-in-config-000"
[tickets]
system = "relay"
server = { url = "https://user:pw-in-url@relay.example/mcp?access_token=query-secret-111&team=core", token_env = "RELAY_TOKEN", env = { RELAY_API_KEY = "env-secret-222", RELAY_PROFILE = "work" } }
[tickets.operations.get]
tool = "get_ticket"
arguments = { ticket = "{id}", apiKey = "argument-secret-333", header = "Authorization: Bearer bearer-secret-444" }
"#,
        );
        let host = layer(Source::Host, "[chief]\nargs = ['--model', 'opus', '--password', 'host-flag-555']");
        let layers = [main, host];
        let tables = layers.iter().map(|l| l.table.clone());
        let effective = Config::from_tables(tables).unwrap();
        let env = |name: &str| match name {
            "RELAY_TOKEN" => Some("token-from-env-666".to_string()),
            "OPENAI_API_KEY" => Some("voice-env-key-777".to_string()),
            _ => None,
        };
        let cleaner = Cleaner::new(secrets(&effective, &layers, env));
        let values = values(&effective, &layers, &cleaner, None);
        let text = serde_json::to_string(&values).unwrap();
        for secret in [
            "sk-agent-flag-123",
            "tok-inline-456",
            "ghp-assignment-789",
            "sk-voice-in-config-000",
            "pw-in-url",
            "query-secret-111",
            "env-secret-222",
            "argument-secret-333",
            "bearer-secret-444",
            "host-flag-555",
            "token-from-env-666",
        ] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        let get = |key: &str| values.get(key).unwrap_or_else(|| panic!("{key} missing"));
        assert_eq!(get("base_ref")["source"], "main");
        assert_eq!(get("layout.sidebar_width")["source"], "default");
        assert_eq!(get("chief.args")["source"], "host");
        assert_eq!(get("chief.args")["value"], serde_json::json!(["--model", "opus", "--password", MASKED]));
        assert_eq!(get("tickets.server.token_env")["value"], "RELAY_TOKEN");
        assert_eq!(get("tickets.server.env.RELAY_PROFILE")["value"], "work");
        assert_eq!(get("tickets.server.env.RELAY_API_KEY")["value"], MASKED);
        assert_eq!(get("tickets.operations.get.arguments.ticket")["value"], "{id}");
        assert_eq!(get("voice.api_key")["value"], MASKED);
        assert_eq!(get("voice.api_key")["source"], "main");
        assert_eq!(get("approvals.env")["value"], serde_json::json!(["GITHUB_TOKEN=***", "CARGO_TERM_COLOR=always"]));
        let url = get("tickets.server.url")["value"].as_str().unwrap();
        assert!(url.contains("relay.example/mcp") && url.contains("team=core"), "{url}");
        let agents = get("agents")["value"].to_string();
        assert!(agents.contains("--model") && agents.contains("opus"), "{agents}");

        let tickets = super::values(&effective, &layers, &cleaner, Some("tickets.server"));
        assert!(!tickets.is_empty() && tickets.keys().all(|key| key.starts_with("tickets.server.")));
    }

    #[test]
    fn parse_errors_name_the_place_without_quoting_the_line() {
        let text = "a = 1\nsecret = sk-unquoted-999\n";
        let error = toml::from_str::<toml::Table>(text).unwrap_err();
        let message = parse_error(text, &error);
        assert!(message.starts_with("line 2, column "), "{message}");
        assert!(!message.contains("sk-unquoted-999"), "{message}");
        let broken = Layer::new(Source::Host, "host.toml", Some(text.into()));
        assert!(broken.table.is_empty() && broken.error.is_some());
        let missing = Layer::new(Source::Host, "host.toml", None);
        assert!(missing.error.is_none() && missing.text.is_none());
    }
}
