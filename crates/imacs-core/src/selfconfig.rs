//! Self-configuration: the chief reads the effective configuration with the source of
//! each value, and proposes changes the user applies. Nothing an agent reads here carries
//! a secret: values of secret-looking environment names, flags and arguments are masked,
//! and every string goes through the credential cleaner built from the secrets the
//! configuration runs with. A proposal is checked and shown as a diff; only [`write`],
//! called when the user applies it, touches a file.

#[cfg(feature = "native")]
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use toml_edit::{Item, TableLike};

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
    /// A repository's allowlisted declarative settings, after the workspace.
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

    /// Reads a file: its text, nothing when it does not exist, or why it could not be read.
    #[cfg(feature = "native")]
    pub fn read(source: Source, file: &Path) -> Layer {
        let path = crate::paths::display(file);
        match std::fs::read_to_string(file) {
            Ok(text) => Layer::new(source, path, Some(text)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Layer::new(source, path, None),
            Err(err) => Layer::unreadable(source, path, err.to_string()),
        }
    }
}

/// Why the main and host files would not load, by file and line and never quoting a line:
/// what an agent may read about a config that fails.
pub fn problems(layers: &[Layer]) -> Vec<String> {
    let mut problems: Vec<String> =
        layers.iter().filter_map(|layer| Some(format!("{}: {}", layer.path, layer.error.as_ref()?))).collect();
    let missing = |layer: &&Layer| layer.source == Source::Main && layer.text.is_none() && layer.error.is_none();
    if let Some(main) = layers.iter().find(missing) {
        problems.push(format!("{} does not exist; IMACS creates it when it starts", main.path));
    }
    problems.extend(load_error(layers));
    problems
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
    // Repo-only hints instruct agents; they have no executable Config fields.
    for layer in layers.iter().filter(|layer| layer.source == Source::Repo) {
        let hints: toml::Table = layer
            .table
            .iter()
            .filter(|(key, _)| matches!(key.as_str(), "pr_title" | "test_commands" | "qa_commands" | "review_policy"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        collect_leaves(&hints, &mut Vec::new(), &mut leaves);
    }
    // The voice key is never serialized; whether it is set in a file is still worth knowing.
    if effective.voice.api_key.is_some() {
        leaves.push((vec!["voice".into(), "api_key".into()], toml::Value::String(MASKED.into())));
    }
    let prefix = prefix.map(str::trim).filter(|p| !p.is_empty());
    leaves
        .into_iter()
        // Every file's prompt adds to the others'; `prompt_sections` shows them, not the last one.
        .filter(|(path, _)| path != &["chief", "prompt"])
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

/// `[chief] prompt` from each file that sets it, in the order the chief's prompt adds them:
/// the main file, this host's, then the workspace's.
pub fn prompt_sections(layers: &[Layer]) -> Vec<(Source, String)> {
    let mut sections: Vec<(Source, String)> = layers
        .iter()
        .filter(|layer| matches!(layer.source, Source::Main | Source::Host | Source::Workspace))
        .filter_map(|layer| {
            let text = lookup(&layer.table, &["chief", "prompt"])?.as_str()?.trim();
            (!text.is_empty()).then(|| (layer.source, text.to_string()))
        })
        .collect();
    sections.sort_by_key(|(source, _)| *source);
    sections
}

/// The source of the value at `path`: the last file that sets it, else the built-in default.
pub fn source_of(layers: &[Layer], path: &[String]) -> Source {
    // A repo overlays individual declarative keys, except workflow, which replaces
    // the section as a whole just like the workspace workflow and tickets.
    if let Some(repo) = layers.iter().rev().find(|layer| layer.source == Source::Repo) {
        if defines(&repo.table, path) {
            return Source::Repo;
        }
        if path.first().is_some_and(|name| name == "workflow") && repo.table.contains_key("workflow") {
            return Source::Default;
        }
    }
    // A workspace's [isolation] merges key by key over the main and host files'.
    if path.first().is_some_and(|name| name == "isolation")
        && layers.iter().rev().any(|layer| layer.source == Source::Workspace && defines(&layer.table, path))
    {
        return Source::Workspace;
    }
    if let Some(section) = path.first().filter(|name| matches!(name.as_str(), "tickets" | "workflow"))
        && let Some(workspace) =
            layers.iter().rev().find(|layer| layer.source == Source::Workspace && layer.table.contains_key(section))
    {
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
    let names: Vec<&str> = path.iter().map(String::as_str).collect();
    let named_secret = match names.as_slice() {
        ["tickets", "server", "env", name] => secret_name(name),
        ["tickets", "operations", _, "arguments", .., name] => secret_name(name) && !placeholder(&value),
        _ => false,
    };
    if named_secret {
        return Value::String(MASKED.into());
    }
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(name, item)| {
                    let mut child_path = path.to_vec();
                    child_path.push(name.clone());
                    (name, masked(&child_path, item))
                })
                .collect(),
        ),
        // Array elements keep their containing field's path: indices do not change
        // whether a nested name is an operation argument or an environment value.
        Value::Array(items) => Value::Array(masked_arguments(path, items)),
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
fn masked_arguments(path: &[String], items: Vec<Value>) -> Vec<Value> {
    let mut out = Vec::with_capacity(items.len());
    let mut mask_next = false;
    for item in items {
        let Value::String(text) = item else {
            mask_next = false;
            out.push(masked(path, item));
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

/// The file a proposed change goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// `config.toml`, shared by every machine that shares the config folder.
    Main,
    /// `config.d/<host>.toml`: this machine only.
    Host,
    /// The active workspace's file, which takes `[tickets]` and `[chief] prompt`.
    Workspace,
    /// `themes/<name>.toml` beside the config: one theme, checked by [`propose_theme`].
    Theme,
}

impl Scope {
    /// The configuration layer the scope's file is; a theme file is none.
    pub fn source(self) -> Option<Source> {
        match self {
            Scope::Main => Some(Source::Main),
            Scope::Host => Some(Source::Host),
            Scope::Workspace => Some(Source::Workspace),
            Scope::Theme => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Scope::Main => "main",
            Scope::Host => "host",
            Scope::Workspace => "workspace",
            Scope::Theme => "theme",
        }
    }
}

/// What a changed setting can make IMACS do, shown with the change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// Programs IMACS starts, keystrokes it types into the editor, servers it connects to,
    /// and which commands agents may run without asking.
    RunsCommands,
    /// Which credential IMACS sends, and where.
    SendsCredentials,
    /// Text agents receive as instructions.
    InstructsAgents,
}

impl Risk {
    pub const ALL: [Risk; 3] = [Risk::RunsCommands, Risk::SendsCredentials, Risk::InstructsAgents];

    pub fn label(self) -> &'static str {
        match self {
            Risk::RunsCommands => "runs commands",
            Risk::SendsCredentials => "sends credentials",
            Risk::InstructsAgents => "instructs agents",
        }
    }

    /// The settings with this risk, as dotted key prefixes.
    fn keys(self) -> &'static [&'static str] {
        match self {
            Risk::RunsCommands => &[
                "agents",
                "approvals",
                "chief.command",
                "chief.args",
                "editor",
                "isolation",
                "tickets.server.command",
                "tickets.server.env",
                "tickets.server.url",
            ],
            Risk::SendsCredentials => {
                &["tickets.server.token_env", "voice.provider", "voice.base_url", "voice.api_key_env", "voice.api_key"]
            }
            Risk::InstructsAgents => &["chief.prompt", "chief.system_prompt", "steer", "tickets.workflow"],
        }
    }

    /// The risk of changing the setting at `key`, if any.
    pub fn of(key: &str) -> Option<Risk> {
        Risk::ALL.into_iter().find(|risk| {
            risk.keys()
                .iter()
                .any(|prefix| key == *prefix || key.strip_prefix(prefix).is_some_and(|r| r.starts_with('.')))
        })
    }
}

/// One setting a proposal adds, changes or removes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Change {
    pub key: String,
    /// `added`, `changed` or `removed`.
    pub change: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk: Option<Risk>,
    #[serde(skip)]
    path: Vec<String>,
}

/// A checked change to one file, waiting for the user.
#[derive(Debug, Clone)]
pub struct Proposal {
    pub scope: Scope,
    /// The file, as shown to the user.
    pub file: String,
    /// The file's text when the change was proposed; `None` for a file it creates.
    pub before: Option<String>,
    pub after: String,
    /// A unified diff of `before` and `after`, for the user only: its context lines may hold secrets.
    pub diff: String,
    pub changes: Vec<Change>,
    /// Why the change may not do what it seems to: unknown keys, overrides that win over it.
    pub warnings: Vec<String>,
}

impl Proposal {
    /// The changed keys of each risk, for the label above the diff.
    pub fn risks(&self) -> Vec<(Risk, Vec<&str>)> {
        Risk::ALL
            .into_iter()
            .map(|risk| {
                (risk, self.changes.iter().filter(|c| c.risk == Some(risk)).map(|c| c.key.as_str()).collect::<Vec<_>>())
            })
            .filter(|(_, keys)| !keys.is_empty())
            .collect()
    }
}

/// Checks a change to one file: `remove` takes dotted keys out, then `fragment` merges
/// in (tables key by key, other values replace). The result must parse and load as a
/// valid configuration, the ticket config included. Nothing is written here.
pub fn propose(scope: Scope, layers: &[Layer], fragment: &str, remove: &[String]) -> Result<Proposal, String> {
    let layer = layers.iter().find(|layer| Some(layer.source) == scope.source()).ok_or_else(|| match scope {
        Scope::Workspace => "no workspace is open: open or save one in IMACS first".to_string(),
        Scope::Theme => "a theme change names its theme".to_string(),
        _ => format!("the {} config file is unknown", scope.name()),
    })?;
    if let Some(error) = &layer.error {
        return Err(format!("{} must be fixed by hand first: {error}", layer.path));
    }
    if fragment.trim().is_empty() && remove.is_empty() {
        return Err("give the change as TOML, keys to remove, or both".into());
    }
    let before = layer.text.as_deref().unwrap_or("");
    let after = edit(before, fragment, remove)?;
    let table: toml::Table = toml::from_str(&after).map_err(|e| parse_error(&after, &e))?;
    let changes = changes(&layer.table, &table);
    if changes.is_empty() {
        return Err(format!("the change sets nothing new in {}", layer.path));
    }
    let mut warnings = Vec::new();
    match scope {
        Scope::Workspace => {
            if let Some(change) = changes.iter().find(|change| !workspace_setting(&change.key)) {
                return Err(format!(
                    "a workspace file takes only [tickets], [workflow], [isolation] and [chief] prompt; {} belongs in the main or host config",
                    change.key
                ));
            }
            load::<crate::workspace::Workspace>(&table, "")
                .map_err(|e| format!("the workspace file would not load: {e}"))?;
            if let Some(prompt) = lookup(&table, &["chief", "prompt"]).filter(|prompt| !prompt.is_str()) {
                return Err(format!("chief.prompt must be text, not {}", prompt.type_str()));
            }
            if let Some(workflow) = table.get("workflow") {
                let workflow = workflow.as_table().ok_or("[workflow] must be a table")?;
                let workflow = load::<crate::workflow::Workflow>(workflow, "workflow.")
                    .map_err(|e| format!("[workflow] would not load: {e}"))?;
                workflow.validate()?;
            }
            if let Some(tickets) = table.get("tickets") {
                let tickets = tickets.as_table().ok_or("[tickets] must be a table")?;
                load::<crate::tickets::TicketConfig>(tickets, "tickets.")
                    .map_err(|e| format!("[tickets] would not load: {e}"))?;
            }
            if let Some(isolation) = table.get("isolation") {
                let isolation = isolation.as_table().ok_or("[isolation] must be a table")?;
                load::<crate::isolation::IsolationConfig>(isolation, "isolation.")
                    .map_err(|e| format!("[isolation] would not load: {e}"))?;
            }
        }
        Scope::Main | Scope::Host => {
            let invalid = |e: String| format!("the config would not load: {e}");
            // The main file is shared: it must load on a machine without this host's overrides too.
            if scope == Scope::Main {
                load::<Config>(&table, "").map_err(invalid)?.workflow.validate()?;
            }
            let tables = layers
                .iter()
                .filter(|layer| matches!(layer.source, Source::Main | Source::Host))
                .map(|layer| if Some(layer.source) == scope.source() { table.clone() } else { layer.table.clone() });
            let config = load::<Config>(&Config::merged(tables), "").map_err(invalid)?;
            config.workflow.validate()?;
            warnings = warnings_for(scope, layers, &config, &changes);
        }
        Scope::Theme => unreachable!("theme changes have no config layer"),
    }
    let diff = unified_diff(&layer.path, before, &after);
    Ok(Proposal { scope, file: layer.path.clone(), before: layer.text.clone(), after, diff, changes, warnings })
}

/// Checks a change to the theme file `themes/<name>.toml`, shown as `file`, whose text is
/// `before` (`None` for a new theme): `remove` takes keys out, then `fragment` merges in.
/// The result must be a theme file with only known keys and usable values that resolves
/// without problems; `read` reads the other theme files it may extend. Nothing is written here.
pub fn propose_theme(
    name: &str,
    file: &str,
    before: Option<&str>,
    fragment: &str,
    remove: &[String],
    read: impl Fn(&str) -> Option<Result<String, String>>,
) -> Result<Proposal, String> {
    if !crate::theme::valid_name(name) {
        return Err(format!("theme names use letters, digits, - and _, not \"{name}\""));
    }
    let old: toml::Table = toml::from_str(before.unwrap_or(""))
        .map_err(|e| format!("{file} must be fixed by hand first: {}", parse_error(before.unwrap_or(""), &e)))?;
    if fragment.trim().is_empty() && remove.is_empty() {
        return Err("give the theme as TOML, keys to remove, or both".into());
    }
    let after = edit(before.unwrap_or(""), fragment, remove)?;
    let table: toml::Table = toml::from_str(&after).map_err(|e| parse_error(&after, &e))?;
    let changes = changes(&old, &table);
    if changes.is_empty() {
        return Err(format!("the change sets nothing new in {file}"));
    }
    let warnings = crate::theme::check(name, &after, read)?;
    let diff = unified_diff(file, before.unwrap_or(""), &after);
    Ok(Proposal {
        scope: Scope::Theme,
        file: file.to_string(),
        before: before.map(str::to_string),
        after,
        diff,
        changes,
        warnings,
    })
}

/// A config file's text with `[theme] name` set, as the theme picker saves it: comments and the
/// rest of the file kept, and a legacy `preset` key taken out, since a file with both does not
/// load. `None` when the file already names that theme.
pub fn with_theme_name(text: &str, name: &str) -> Result<Option<String>, String> {
    if !crate::theme::valid_name(name) {
        return Err(format!("\"{name}\" is not a theme name"));
    }
    let table: toml::Table =
        toml::from_str(text).map_err(|e| format!("it must be fixed by hand first: {}", parse_error(text, &e)))?;
    let legacy = lookup(&table, &["theme", "preset"]).is_some();
    let remove = if legacy { vec!["theme.preset".to_string()] } else { Vec::new() };
    let after = edit(text, &format!("[theme]\nname = \"{name}\""), &remove)?;
    Ok((after != text).then_some(after))
}

/// Whether a config file's text picks a theme, by name or with the legacy `preset` key.
pub fn picks_theme(text: &str) -> bool {
    let table = toml::from_str::<toml::Table>(text).unwrap_or_default();
    lookup(&table, &["theme", "name"]).or_else(|| lookup(&table, &["theme", "preset"])).is_some()
}

fn unified_diff(file: &str, before: &str, after: &str) -> String {
    similar::TextDiff::from_lines(before, after).unified_diff().context_radius(3).header(file, file).to_string()
}

/// Deserializes a table, naming the key an error points at, after `prefix`. Values carry no
/// positions, so on an error the table is written out and read again to find the key.
fn load<T: serde::de::DeserializeOwned>(table: &toml::Table, prefix: &str) -> Result<T, String> {
    toml::Value::Table(table.clone()).try_into::<T>().map_err(|error| {
        let message = error.message().trim().to_string();
        let key = toml::to_string(table).ok().and_then(|text| {
            let start = toml::from_str::<T>(&text).err()?.span()?.start;
            key_at(&text, start)
        });
        match key {
            Some(key) => format!("{prefix}{key}: {message}"),
            None => message,
        }
    })
}

/// The dotted key of the line at `offset`, in TOML as the serializer writes it.
fn key_at(text: &str, offset: usize) -> Option<String> {
    let start = text[..offset.min(text.len())].rfind('\n').map_or(0, |at| at + 1);
    let name = text[start..].lines().next()?.split_once('=')?.0.trim();
    let header = text[..start].lines().rev().find(|line| line.starts_with('['));
    Some(match header.map(|line| line.trim_matches(['[', ']'])) {
        Some(table) => format!("{table}.{name}"),
        None => name.to_string(),
    })
}

/// Why the files that parse would not load together: the main file with this host's,
/// and the workspace's `[tickets]`. Files that do not parse report their own errors.
pub fn load_error(layers: &[Layer]) -> Option<String> {
    let files = || layers.iter().filter(|layer| matches!(layer.source, Source::Main | Source::Host));
    if files().any(|layer| layer.error.is_some()) {
        return None;
    }
    let merged = Config::merged(files().map(|layer| layer.table.clone()));
    if let Err(error) = load::<Config>(&merged, "") {
        return Some(format!("the config does not load: {error}"));
    }
    if let Some(workflow) =
        layers.iter().filter(|layer| layer.source == Source::Workspace).find_map(|l| l.table.get("workflow"))
    {
        let parsed: Result<crate::workflow::Workflow, _> = workflow.clone().try_into();
        if let Some(error) =
            parsed.map_err(|_| "invalid workflow settings".to_string()).and_then(|workflow| workflow.validate()).err()
        {
            return Some(format!("the workspace's [workflow] does not load: {error}"));
        }
    }
    let tickets = layers.iter().filter(|layer| layer.source == Source::Workspace).find_map(|l| l.table.get("tickets"));
    match tickets.map(|tickets| tickets.as_table().ok_or_else(|| "must be a table".to_string())) {
        Some(Ok(tickets)) => load::<crate::tickets::TicketConfig>(tickets, "tickets.").err(),
        Some(Err(error)) => Some(error),
        None => None,
    }
    .map(|error| format!("the workspace's [tickets] does not load: {error}"))
}

/// The settings a workspace file carries.
fn workspace_setting(key: &str) -> bool {
    ["tickets", "workflow", "isolation"]
        .iter()
        .any(|section| key == *section || key.strip_prefix(section).is_some_and(|rest| rest.starts_with('.')))
        || key == "chief.prompt"
}

/// Every setting that differs between two versions of a file.
fn changes(before: &toml::Table, after: &toml::Table) -> Vec<Change> {
    let (mut old, mut new) = (Vec::new(), Vec::new());
    collect_leaves(before, &mut Vec::new(), &mut old);
    collect_leaves(after, &mut Vec::new(), &mut new);
    let change = |path: &Vec<String>, change| {
        let key = key(path);
        Change { risk: Risk::of(&key), key, change, path: path.clone() }
    };
    let mut changes = Vec::new();
    for (path, value) in &new {
        match old.iter().find(|(old, _)| old == path) {
            None => changes.push(change(path, "added")),
            Some((_, old)) if old != value => changes.push(change(path, "changed")),
            Some(_) => {}
        }
    }
    for (path, _) in old.iter().filter(|(path, _)| !new.iter().any(|(new, _)| new == path)) {
        changes.push(change(path, "removed"));
    }
    changes
}

/// Reasons a valid change may not do what it seems to.
fn warnings_for(scope: Scope, layers: &[Layer], config: &Config, changes: &[Change]) -> Vec<String> {
    let known = [toml::Table::try_from(config), toml::Table::try_from(Config::default())];
    let host = layers.iter().find(|layer| layer.source == Source::Host);
    let workspace_tickets =
        layers.iter().any(|layer| layer.source == Source::Workspace && layer.table.contains_key("tickets"));
    let mut warnings = Vec::new();
    for change in changes.iter().filter(|change| change.change != "removed") {
        let known = known.iter().flatten().any(|table| defines(table, &change.path));
        if !known && change.key != "voice.api_key" {
            warnings.push(format!("IMACS does not know {}, so it would have no effect", change.key));
        }
        if change.key == "voice.api_key" {
            warnings.push("voice.api_key puts a key in the file; api_key_env names a variable instead".into());
        }
        if let Some(host) = host.filter(|host| scope == Scope::Main && defines(&host.table, &change.path)) {
            warnings.push(format!("{} sets {} too and wins on this machine", host.path, change.key));
        }
    }
    if workspace_tickets && changes.iter().any(|change| change.path[0] == "tickets") {
        warnings.push("the open workspace's [tickets] replaces this section while it is open".into());
    }
    warnings
}

/// The file's text with the keys in `remove` taken out and `fragment` merged in, keeping
/// comments and layout: a replaced value keeps the comments above its key and after it.
pub fn edit(text: &str, fragment: &str, remove: &[String]) -> Result<String, String> {
    let mut document: toml_edit::DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| edit_error(text, &e))?;
    let mut change: toml_edit::DocumentMut =
        fragment.parse().map_err(|e: toml_edit::TomlError| format!("the change is not valid TOML: {e}"))?;
    for key in remove {
        let path = toml_edit::Key::parse(key.trim())
            .map_err(|e| format!("{key:?} is not a TOML key: {}", e.message().trim()))?;
        if remove_key(document.as_table_mut(), &path).is_none() {
            return Err(format!("{key} is not set in this file"));
        }
    }
    for (_, item) in change.as_table_mut().iter_mut() {
        detach(item);
    }
    merge(document.as_table_mut(), change.as_table(), false);
    let after = document.to_string();
    Ok(if text.trim().is_empty() { after.trim_start().to_string() } else { after })
}

/// A parse error by line and column, without the source line.
fn edit_error(text: &str, error: &toml_edit::TomlError) -> String {
    match error.span() {
        Some(span) => {
            let before = &text[..span.start.min(text.len())];
            let line = before.matches('\n').count() + 1;
            format!("line {line}: {}", error.message().trim())
        }
        None => error.message().trim().to_string(),
    }
}

fn remove_key(table: &mut dyn TableLike, path: &[toml_edit::Key]) -> Option<Item> {
    match path {
        [last] => table.remove(last.get()),
        [first, rest @ ..] => remove_key(table.get_mut(first.get())?.as_table_like_mut()?, rest),
        [] => None,
    }
}

/// Merges `source` into `target`: tables key by key, anything else replaced in place, so
/// the key keeps its comments and a replaced value its trailing comment. Inside an inline
/// table everything becomes a value.
fn merge(target: &mut dyn TableLike, source: &dyn TableLike, inline: bool) {
    let mut inserted = false;
    for (name, item) in source.iter() {
        match target.get_mut(name) {
            Some(existing) if existing.is_table_like() && item.is_table_like() => {
                let inline = inline || existing.is_inline_table();
                if let (Some(target), Some(source)) = (existing.as_table_like_mut(), item.as_table_like()) {
                    merge(target, source, inline);
                }
            }
            Some(existing) => {
                let mut item = item.clone();
                if inline {
                    item.make_value();
                }
                match (&*existing, &mut item) {
                    (Item::Value(old), Item::Value(new)) => *new.decor_mut() = old.decor().clone(),
                    (Item::ArrayOfTables(old), Item::ArrayOfTables(new)) => {
                        if let (Some(old), Some(new)) = (old.get(0), new.get_mut(0)) {
                            *new.decor_mut() = old.decor().clone();
                        }
                    }
                    _ => {}
                }
                *existing = item;
            }
            None => {
                target.insert(name, item.clone());
                // A new key keeps the comment written above it in the change.
                if let (Some(mut key), Some(written)) = (target.key_mut(name), source.key(name)) {
                    *key.leaf_decor_mut() = written.leaf_decor().clone();
                }
                inserted = true;
            }
        }
    }
    // An inline table with a new key is spaced evenly again; its own trailing comment stays.
    if inline && inserted {
        target.fmt();
    }
}

/// Tables from a change go where they are inserted, not where they stood in the change,
/// and start after a blank line.
fn detach(item: &mut Item) {
    let tables: Vec<&mut toml_edit::Table> = match item {
        Item::Table(table) => vec![table],
        Item::ArrayOfTables(array) => array.iter_mut().collect(),
        _ => Vec::new(),
    };
    for table in tables {
        table.set_position(None);
        let prefix = table.decor().prefix().and_then(|p| p.as_str()).unwrap_or("").to_string();
        if !table.is_implicit() && !table.is_dotted() && !prefix.starts_with('\n') {
            table.decor_mut().set_prefix(format!("\n{prefix}"));
        }
        for (_, item) in table.iter_mut() {
            detach(item);
        }
    }
}

/// Writes an applied change, only while the file still reads as it did when the change was
/// proposed: first a copy of the old file next to it, then the new text through a temporary
/// file in the same folder, with the old file's permissions. A symlinked file is written
/// through its link. Returns the backup's path, `None` for a file the change creates.
#[cfg(feature = "native")]
pub fn write(file: &Path, before: Option<&str>, after: &str, stamp: u64) -> anyhow::Result<Option<PathBuf>> {
    use anyhow::{Context, bail};
    let current = match std::fs::read_to_string(file) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err).with_context(|| format!("reading {}", file.display())),
    };
    if current.as_deref() != before {
        bail!("{} changed since the change was proposed; ask for a new proposal", file.display());
    }
    let target = if current.is_some() { std::fs::canonicalize(file)? } else { file.to_path_buf() };
    let (Some(dir), Some(name)) = (target.parent(), target.file_name()) else {
        bail!("{} is not a file", file.display());
    };
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let backup = match current {
        Some(_) => {
            let shown = file.file_name().unwrap_or(name).to_string_lossy().into_owned();
            let folder = file.parent().unwrap_or(dir);
            let mut backup = folder.join(format!("{shown}.{stamp}.bak"));
            let mut n = 1;
            while backup.exists() {
                backup = folder.join(format!("{shown}.{stamp}-{n}.bak"));
                n += 1;
            }
            std::fs::copy(&target, &backup).with_context(|| format!("backing up to {}", backup.display()))?;
            Some(backup)
        }
        None => None,
    };
    let temp = dir.join(format!(".{}.{stamp}.imacs-tmp", name.to_string_lossy()));
    let written = (|| -> anyhow::Result<()> {
        std::fs::write(&temp, after)?;
        if backup.is_some() {
            std::fs::set_permissions(&temp, std::fs::metadata(&target)?.permissions())?;
        }
        std::fs::rename(&temp, &target)?;
        Ok(())
    })();
    if let Err(err) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(err).with_context(|| format!("writing {}", file.display()));
    }
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(source: Source, text: &str) -> Layer {
        Layer::new(source, format!("{source:?}.toml"), Some(text.into()))
    }

    #[test]
    fn the_theme_picker_sets_the_name_and_drops_a_legacy_preset() {
        let config = "# shared\n[theme]\nname = \"claude\" # the default\n[font]\nsize = 14.0\n";
        let picked = with_theme_name(config, "atomisk").unwrap().unwrap();
        assert_eq!(picked, "# shared\n[theme]\nname = \"atomisk\" # the default\n[font]\nsize = 14.0\n");
        assert_eq!(with_theme_name(&picked, "atomisk").unwrap(), None, "nothing to write");
        assert_eq!(with_theme_name("", "tide").unwrap().unwrap(), "[theme]\nname = \"tide\"\n");
        let legacy = with_theme_name("[theme]\npreset = \"midnight\"\naccent = \"#00ff88\"\n", "paper").unwrap();
        assert_eq!(legacy.unwrap(), "[theme]\naccent = \"#00ff88\"\nname = \"paper\"\n");
        assert!(with_theme_name("[theme\n", "paper").unwrap_err().contains("fixed by hand"));
        assert!(with_theme_name("", "../x").is_err());
        assert!(picks_theme("[theme]\nname = \"x\"") && picks_theme("[theme]\npreset = \"x\""));
        assert!(!picks_theme("[theme]\naccent = \"#000000\"") && !picks_theme("[theme"));
    }

    #[test]
    fn theme_changes_are_checked_as_theme_files() {
        let read = |name: &str| (name == "base").then(|| Ok("extends = \"midnight\"\n".to_string()));
        let file = "~/.config/imacs/themes/dusk.toml";
        let new = propose_theme("dusk", file, None, "extends = \"base\"\naccent = \"#e0a458\"", &[], read).unwrap();
        assert_eq!(new.scope, Scope::Theme);
        assert_eq!(new.after, "extends = \"base\"\naccent = \"#e0a458\"\n");
        assert!(new.before.is_none() && new.warnings.is_empty());
        let keys: Vec<_> = new.changes.iter().map(|c| (c.key.as_str(), c.change, c.risk)).collect();
        assert_eq!(keys, [("accent", "added", None), ("extends", "added", None)]);
        assert!(new.diff.contains("+accent = \"#e0a458\""), "{}", new.diff);

        let before = "extends = \"base\"\n# Warm dusk\naccent = \"#e0a458\" # amber\nraised = \"#2a2620\"\n";
        let changed =
            propose_theme("dusk", file, Some(before), "accent = \"#d08c3a\"", &["raised".into()], read).unwrap();
        assert_eq!(changed.after, "extends = \"base\"\n# Warm dusk\naccent = \"#d08c3a\" # amber\n", "comments stay");
        let shadow = propose_theme("claude", file, None, "extends = \"claude\"\naccent = \"#e0a458\"", &[], read);
        assert_eq!(shadow.unwrap().warnings, ["themes/claude.toml replaces the built-in claude theme"]);
        for (name, before, fragment, problem) in [
            ("dusk", None, "acent = \"#e0a458\"", "unknown key acent"),
            ("dusk", None, "accent = \"amber\"", "is not a #rrggbb color"),
            ("dusk", None, "extends = \"nope\"", "unknown theme"),
            ("dusk", None, "mode = \"sepia\"", "themes/dusk.toml"),
            ("dusk", Some(before), "accent = \"#e0a458\"", "nothing new"),
            ("dusk", Some("accent = "), "accent = \"#e0a458\"", "fixed by hand"),
            ("dusk", None, "", "give the theme"),
            ("../dusk", None, "accent = \"#e0a458\"", "theme names use"),
        ] {
            let error = propose_theme(name, file, before, fragment, &[], read).unwrap_err();
            assert!(error.contains(problem), "{fragment}: {error}");
        }
        let error = propose(Scope::Theme, &[], "accent = \"#e0a458\"", &[]).unwrap_err();
        assert!(error.contains("names its theme"), "{error}");
    }

    #[test]
    fn workspace_workflow_proposals_validate_and_sources_include_repo() {
        let main = layer(Source::Main, "[workflow]\ndone = 'merged'\n");
        let host = layer(Source::Host, "[workflow]\ndone = 'ci_passed'\n");
        let workspace = layer(Source::Workspace, "projects = []\n");
        let layers = [main, host, workspace];
        let proposal = propose(
            Scope::Workspace,
            &layers,
            "[workflow]\ndone = 'qa_passed'\nstages = ['accepted', 'qa_passed']",
            &[],
        )
        .unwrap();
        assert!(proposal.after.contains("qa_passed"));
        assert!(propose(Scope::Workspace, &layers, "[workflow]\ndone = 'ci_failed'", &[]).is_err());
        assert!(propose(Scope::Workspace, &layers, "[workflow]\nstages = ['merged']", &[]).is_err());
        assert!(propose(Scope::Workspace, &layers, "[workflow]\ndone = 42", &[]).is_err());
        let path = |key: &str| key.split('.').map(str::to_string).collect::<Vec<_>>();
        assert_eq!(source_of(&layers, &path("workflow.done")), Source::Host);
        let mut layers = layers.to_vec();
        layers[2] = layer(Source::Workspace, "projects = []\n[workflow]\ndone = 'pr_opened'");
        assert_eq!(source_of(&layers, &path("workflow.done")), Source::Workspace);
        assert_eq!(source_of(&layers, &path("workflow.stages")), Source::Default);
        layers.push(layer(
            Source::Repo,
            "[workflow]\nstages = ['accepted', 'merged']\n[tickets]\nbranch = 'repo/{slug}'",
        ));
        assert_eq!(source_of(&layers, &path("workflow.done")), Source::Default);
        assert_eq!(source_of(&layers, &path("workflow.stages")), Source::Repo);
        assert_eq!(source_of(&layers, &path("tickets.branch")), Source::Repo);
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
    fn values_mask_nested_operation_arguments_without_losing_their_path() {
        let main = layer(
            Source::Main,
            r#"
[[agents]]
name = "custom"
command = ["agent", "--password", "agent-secret", "--model", "public"]
[approvals]
env = ["SERVICE_TOKEN=approval-secret", "PROFILE=work"]
[tickets.operations.get]
tool = "get_ticket"
[tickets.operations.get.arguments]
direct = { password = "direct-secret", name = "public" }
requests = [{ api_key = "request-secret", ticket_key = "{key}", nested = { batches = [[{ credential = "deep-secret", public = 7, command = ["tool", "--token", "flag-secret"] }]] } }]
[[tickets.operations.get.arguments.tables]]
secret = "table-secret"
children = [{ auth = { header = "object-secret" }, label = "public" }]
"#,
        );
        assert!(main.error.is_none(), "{:?}", main.error);
        let effective = Config::from_tables([main.table.clone()]).unwrap();
        // None of these secrets is known to the cleaner: the field names and paths
        // must protect them even inside arrays and inline tables.
        let values = values(&effective, &[main], &Cleaner::default(), None);
        let get = |key: &str| &values[key]["value"];
        assert_eq!(get("tickets.operations.get.arguments.direct.password"), MASKED);
        let request = &get("tickets.operations.get.arguments.requests")[0];
        assert_eq!(request["api_key"], MASKED);
        assert_eq!(request["ticket_key"], "{key}");
        let deep = &request["nested"]["batches"][0][0];
        assert_eq!(deep["credential"], MASKED);
        assert_eq!(deep["public"], 7);
        assert_eq!(deep["command"], serde_json::json!(["tool", "--token", MASKED]));
        let table = &get("tickets.operations.get.arguments.tables")[0];
        assert_eq!(table["secret"], MASKED);
        assert_eq!(table["children"][0]["auth"], MASKED);
        assert_eq!(table["children"][0]["label"], "public");
        assert_eq!(
            get("agents")[0]["command"],
            serde_json::json!(["agent", "--password", MASKED, "--model", "public"])
        );
        assert_eq!(get("approvals.env"), &serde_json::json!(["SERVICE_TOKEN=***", "PROFILE=work"]));
        let text = serde_json::to_string(&values).unwrap();
        for secret in [
            "direct-secret",
            "request-secret",
            "deep-secret",
            "flag-secret",
            "table-secret",
            "object-secret",
            "agent-secret",
            "approval-secret",
        ] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
    }

    #[test]
    fn command_masking_walks_nested_tables_and_arrays_in_other_sections() {
        let value = serde_json::json!({
            "groups": [[{ "nested": { "command": ["tool", "--api-key=inline-secret", "PASSWORD=assigned-secret", "--token", "flag-secret", "public"] } }]],
            "token_env": "SERVICE_TOKEN",
            "key": "public-key",
        });
        assert_eq!(
            masked(&["other".into()], value),
            serde_json::json!({
                "groups": [[{ "nested": { "command": ["tool", "--api-key=***", "PASSWORD=***", "--token", MASKED, "public"] } }]],
                "token_env": "SERVICE_TOKEN",
                "key": "public-key",
            }),
        );
    }

    #[test]
    fn edits_keep_comments_and_layout() {
        use crate::config::DEFAULT_CONFIG;
        let edited = edit(
            DEFAULT_CONFIG,
            "base_ref = \"main\"\n[layout]\nmin_tile_width = 400.0\n[chief]\nheartbeat_secs = 60",
            &[],
        )
        .unwrap();
        let expected = DEFAULT_CONFIG
            .replace("base_ref = \"origin/main\"", "base_ref = \"main\"")
            .replace("min_tile_width = 520.0 ", "min_tile_width = 400.0 ")
            .replace("heartbeat_secs = 300 ", "heartbeat_secs = 60 ");
        assert_eq!(edited, expected);
        assert!(edited.contains("# Every diff is taken against the merge-base of this ref and the worktree's HEAD,\n"));
        assert!(edited.contains("min_tile_width = 400.0      # below this a terminal tile collapses into a tab"));

        // Arrays replace whole, new tables go at the end, and a change's own comments stay.
        let edited = edit(
            DEFAULT_CONFIG,
            "# Work Relay\n[tickets]\nsystem = \"relay\"\n# The bridge\nserver = { command = [\"relay-mcp\"] }\n\
             [[agents]]\nname = \"claude\"\ncommand = [\"claude\"]\nkind = \"claude\"",
            &[],
        )
        .unwrap();
        let tail = &edited[edited.find("[keys]").unwrap()..];
        assert!(tail.contains("\n\n# Work Relay\n[tickets]\nsystem = \"relay\"\n# The bridge\nserver = "), "{tail}");
        assert!(
            edited.contains(
                "# Agents you can start. kind = \"claude\" turns on hooks, session ids and resume.\n[[agents]]"
            )
        );
        assert_eq!(edited.matches("[[agents]]").count(), 1, "{edited}");
        let config: Config = toml::from_str(&edited).unwrap();
        assert_eq!(config.agents.len(), 1);
        assert_eq!(config.tickets.system, "relay");

        // Keys merge into inline tables; removals happen first and drop the key's comment.
        let text = "# Profile\n[tickets]\nsystem = 'relay'\nserver = { command = ['a'] }  # bridge\n";
        let edited = edit(text, "[tickets.server.env]\nPROFILE = 'work'", &["tickets.system".into()]).unwrap();
        assert_eq!(
            edited,
            "# Profile\n[tickets]\nserver = { command = ['a'], env = { PROFILE = 'work' } }  # bridge\n"
        );
        assert!(edit(text, "", &["tickets.missing".into()]).unwrap_err().contains("not set"));
        assert!(edit(text, "[tickets", &[]).unwrap_err().contains("not valid TOML"));
        assert_eq!(edit("", "[layout]\nsidebar_width = 300.0", &[]).unwrap(), "[layout]\nsidebar_width = 300.0\n");
    }

    #[test]
    fn proposals_validate_label_and_warn() {
        let main = layer(Source::Main, "base_ref = 'main'\n[layout]\nsidebar_width = 300.0\n");
        let host = layer(Source::Host, "[layout]\nsidebar_width = 320.0\n");
        let layers = [main, host];
        let error = propose(Scope::Main, &layers, "[layout]\nsidebar_width = 'wide'", &[]).unwrap_err();
        assert_eq!(
            error,
            "the config would not load: layout.sidebar_width: invalid type: string \"wide\", expected f32"
        );
        let error = propose(Scope::Host, &layers, "[tickets]\nmode = 'sideways'", &[]).unwrap_err();
        assert!(error.starts_with("the config would not load: tickets.mode: unknown variant"), "{error}");
        assert!(propose(Scope::Main, &layers, "base_ref = 'main'", &[]).unwrap_err().contains("nothing new"));
        assert!(
            propose(Scope::Workspace, &layers, "[tickets]\nsystem = 'x'", &[]).unwrap_err().contains("no workspace")
        );

        let proposal = propose(
            Scope::Main,
            &layers,
            "[layout]\nsidebar_width = 280.0\nsidebar_widht = 1.0\n[[agents]]\nname = 'aider'\ncommand = ['aider']\n\
             [voice]\nbase_url = 'https://audio.example/v1'\n[chief]\nsystem_prompt = 'Be brief.'",
            &[],
        )
        .unwrap();
        let risk = |key: &str| proposal.changes.iter().find(|c| c.key == key).unwrap().risk;
        assert_eq!(risk("agents"), Some(Risk::RunsCommands));
        assert_eq!(risk("voice.base_url"), Some(Risk::SendsCredentials));
        assert_eq!(risk("chief.system_prompt"), Some(Risk::InstructsAgents));
        assert_eq!(risk("layout.sidebar_width"), None);
        assert_eq!(proposal.risks()[0], (Risk::RunsCommands, vec!["agents"]));
        assert!(
            proposal.warnings.iter().any(|w| w.contains("does not know layout.sidebar_widht")),
            "{:?}",
            proposal.warnings
        );
        assert!(
            proposal.warnings.iter().any(|w| w.contains("Host.toml sets layout.sidebar_width")),
            "{:?}",
            proposal.warnings
        );
        assert!(proposal.diff.starts_with("--- Main.toml\n+++ Main.toml\n@@"), "{}", proposal.diff);
        assert!(proposal.diff.contains("-sidebar_width = 300.0\n+sidebar_width = 280.0\n"), "{}", proposal.diff);
        assert_eq!(proposal.before.as_deref(), Some("base_ref = 'main'\n[layout]\nsidebar_width = 300.0\n"));
        for (key, risky) in [
            ("tickets.server.url", true),
            ("tickets.server.env.TOKEN", true),
            ("approvals.allow", true),
            ("editor.startup_keys", true),
            ("steer.session_context", true),
            ("tickets.servers", false),
            ("agents_extra", false),
            ("layout.sidebar_width", false),
        ] {
            assert_eq!(Risk::of(key).is_some(), risky, "{key}");
        }

        // A removal falls back to the file below; a new host file starts empty.
        let proposal = propose(Scope::Host, &layers, "", &["layout.sidebar_width".into()]).unwrap();
        assert_eq!(proposal.changes[0].change, "removed");
        let fresh = [layers[0].clone(), Layer::new(Source::Host, "Host.toml", None)];
        let proposal = propose(Scope::Host, &fresh, "[font]\nsize = 15.0", &[]).unwrap();
        assert_eq!((proposal.before.as_deref(), proposal.after.as_str()), (None, "[font]\nsize = 15.0\n"));

        // Workspace files take only their own sections, and their tickets must load.
        let workspace = layer(Source::Workspace, "projects = ['~/repos/app']\n");
        let layers = [layers[0].clone(), layers[1].clone(), workspace];
        let error = propose(Scope::Workspace, &layers, "[layout]\nsidebar_width = 1.0", &[]).unwrap_err();
        assert!(error.contains("only [tickets], [workflow], [isolation] and [chief] prompt"), "{error}");
        assert!(propose(Scope::Workspace, &layers, "projects = []", &[]).is_err());
        let error = propose(Scope::Workspace, &layers, "[isolation]\nmode = 'open'", &[]).unwrap_err();
        assert!(error.starts_with("[isolation] would not load: isolation.mode: unknown variant"), "{error}");
        let proposal = propose(Scope::Workspace, &layers, "[isolation]\nmode = 'bare'", &[]).unwrap();
        assert_eq!(proposal.risks(), [(Risk::RunsCommands, vec!["isolation.mode"])]);
        let with_isolation = [
            layers[0].clone(),
            layer(Source::Host, "[isolation.sandbox]\nmemory = '8g'\ncpus = 4"),
            layer(Source::Workspace, "projects = []\n[isolation]\nmode = 'bare'\n[isolation.sandbox]\ncpus = 2"),
        ];
        for (key, source) in [
            ("mode", Source::Workspace),
            ("sandbox.cpus", Source::Workspace),
            ("sandbox.memory", Source::Host),
            ("sandbox.ssh_agent", Source::Default),
        ] {
            let path: Vec<String> = format!("isolation.{key}").split('.').map(String::from).collect();
            assert_eq!(source_of(&with_isolation, &path), source, "{key}");
        }
        let error = propose(Scope::Workspace, &layers, "[tickets]\ncomments = 'loud'", &[]).unwrap_err();
        assert!(error.starts_with("[tickets] would not load: tickets.comments: unknown variant"), "{error}");
        let proposal = propose(
            Scope::Workspace,
            &layers,
            "[tickets]\nsystem = 'relay'\nserver = { command = ['relay-mcp'] }",
            &[],
        )
        .unwrap();
        assert_eq!(proposal.risks(), [(Risk::RunsCommands, vec!["tickets.server.command"])]);
        let quiet = propose(Scope::Main, &layers, "[tickets]\nsystem = 'relay'", &[]).unwrap();
        assert!(quiet.warnings.is_empty(), "{:?}", quiet.warnings);
        let mut layers = layers;
        layers[2] = Layer::new(Source::Workspace, "Workspace.toml", Some(proposal.after.clone()));
        let shadowed = propose(Scope::Main, &layers, "[tickets]\nsystem = 'relay'", &[]).unwrap();
        assert!(
            shadowed.warnings.iter().any(|w| w.contains("workspace's [tickets] replaces")),
            "{:?}",
            shadowed.warnings
        );

        // A file that does not parse is fixed by hand, never rewritten.
        let broken = [Layer::new(Source::Main, "Main.toml", Some("[layout\nsecret = 'sk-1'".into()))];
        let error = propose(Scope::Main, &broken, "base_ref = 'x'", &[]).unwrap_err();
        assert!(error.contains("fixed by hand") && !error.contains("sk-1"), "{error}");
    }

    #[cfg(feature = "native")]
    #[test]
    fn writing_checks_the_file_backs_it_up_and_keeps_its_mode_and_link() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_support::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        std::fs::write(&file, "a = 1\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = write(&file, Some("a = 0\n"), "a = 2\n", 7).unwrap_err().to_string();
        assert!(error.contains("changed since"), "{error}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a = 1\n");
        let backup = write(&file, Some("a = 1\n"), "a = 2\n", 7).unwrap().unwrap();
        assert_eq!(backup, dir.path().join("config.toml.7.bak"));
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "a = 1\n");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a = 2\n");
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777, 0o600);
        let second = write(&file, Some("a = 2\n"), "a = 3\n", 7).unwrap().unwrap();
        assert_eq!(second, dir.path().join("config.toml.7-1.bak"));

        // A symlinked config stays a link; its target gets the change.
        let real = dir.path().join("dotfiles.toml");
        std::fs::write(&real, "b = 1\n").unwrap();
        let link = dir.path().join("linked.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write(&link, Some("b = 1\n"), "b = 2\n", 8).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "b = 2\n");

        let new = dir.path().join("config.d/host.toml");
        assert_eq!(write(&new, None, "c = 1\n", 9).unwrap(), None);
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "c = 1\n");
        assert!(write(&new, None, "c = 2\n", 9).is_err(), "a file created meanwhile is not overwritten");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("imacs-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn chief_prompt_sections_go_main_host_workspace() {
        let workspace = layer(Source::Workspace, "projects = []\n[chief]\nprompt = 'Workspace rule.'");
        let host = layer(Source::Host, "[chief]\nprompt = '''\n  Host rule.\n'''");
        let main = layer(Source::Main, "[chief]\nprompt = 'Main rule.'\nargs = []");
        let blank = layer(Source::Main, "[chief]\nprompt = '  '");
        let sections = prompt_sections(&[workspace.clone(), host.clone(), main.clone()]);
        assert_eq!(
            sections,
            [
                (Source::Main, "Main rule.".to_string()),
                (Source::Host, "Host rule.".to_string()),
                (Source::Workspace, "Workspace rule.".to_string())
            ]
        );
        assert_eq!(prompt_sections(&[blank, host.clone()]), [(Source::Host, "Host rule.".to_string())]);
        // The merged value is only the last file's, so it is not shown as one.
        let effective = Config::from_tables([main.table.clone(), host.table.clone()]).unwrap();
        let values = values(&effective, &[main.clone(), host], &Cleaner::default(), Some("chief"));
        assert!(values.contains_key("chief.args") && !values.contains_key("chief.prompt"), "{values:?}");

        let layers = [main, layer(Source::Workspace, "projects = []")];
        let proposal = propose(Scope::Workspace, &layers, "[chief]\nprompt = 'Use the team board.'", &[]).unwrap();
        assert_eq!(proposal.risks(), [(Risk::InstructsAgents, vec!["chief.prompt"])]);
        let error = propose(Scope::Workspace, &layers, "[chief]\nprompt = 3", &[]).unwrap_err();
        assert!(error.contains("chief.prompt must be text"), "{error}");
        assert!(propose(Scope::Workspace, &layers, "[chief]\nargs = ['x']", &[]).is_err());
        let error = propose(Scope::Main, &layers, "[chief]\nprompt = ['x']", &[]).unwrap_err();
        assert!(error.contains("chief.prompt: invalid type"), "{error}");
    }

    #[test]
    fn documented_labels_match_the_labeled_settings() {
        let docs = include_str!("../../../docs/configuration.md");
        for risk in Risk::ALL {
            let keys = risk.keys().iter().map(|key| format!("`{key}`")).collect::<Vec<_>>().join(", ");
            let row = format!("| {} | {keys} |", risk.label());
            assert!(docs.contains(&row), "{row} is missing from docs/configuration.md");
        }
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
