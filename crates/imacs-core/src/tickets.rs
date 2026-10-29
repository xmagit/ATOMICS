//! Generic ticket links. A plan or task can carry a ticket from an external ticket
//! system that IMACS reaches over MCP. Nothing here knows any particular system: ids
//! and states are opaque strings, and the workspace's `[tickets]` config says which
//! MCP tool implements each operation and where its result keeps the ticket's fields.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::plan::{Plan, PlanStatus, Task, TaskStatus};

/// A ticket attached to a plan (a board, or the plan's one ticket) or to a task.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TicketLink {
    /// The ticket system's name from the config, such as the one in `[tickets] system`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub system: String,
    /// What the configured operations address the ticket by.
    pub id: String,
    /// The human-readable key shown on the card, when it differs from the id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The ticket system's suggested git branch, when supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The system's own state name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// Unix time IMACS last read the ticket.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub synced_at: Option<u64>,
    /// The state IMACS last pushed. IMACS pushes again only when its own status
    /// changes, so a state someone set in the ticket system is never overwritten
    /// behind their back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pushed_state: Option<String>,
    /// Why the last ticket operation failed; cleared by the next success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl TicketLink {
    /// The key when known, else the id.
    pub fn label(&self) -> &str {
        self.key.as_deref().filter(|k| !k.is_empty()).unwrap_or(&self.id)
    }

    /// Whether a ticket reference (any of system, id or key) names this ticket.
    pub fn matches(&self, system: Option<&str>, id: Option<&str>, key: Option<&str>) -> bool {
        let system_ok = system.is_none_or(|s| self.system.is_empty() || s.is_empty() || s == self.system);
        let names = [Some(self.id.as_str()), self.key.as_deref()];
        let named = |reference: Option<&str>| {
            reference
                .filter(|r| !r.is_empty())
                .is_some_and(|r| names.iter().flatten().any(|n| !n.is_empty() && *n == r))
        };
        system_ok && (named(id) || named(key))
    }

    /// Whether a fetched ticket is this one.
    pub fn same_ticket(&self, other: &TicketLink) -> bool {
        self.matches(Some(&other.system), Some(&other.id), other.key.as_deref())
    }

    /// Takes the fields a ticket operation returned. The id stays what the link was
    /// made with, since that is what the operations address. Returns whether anything changed.
    pub fn update_from(&mut self, fetched: &TicketLink) -> bool {
        let before = self.clone();
        if self.id.is_empty() {
            self.id = fetched.id.clone();
        }
        for (field, value) in [
            (&mut self.key, &fetched.key),
            (&mut self.url, &fetched.url),
            (&mut self.title, &fetched.title),
            (&mut self.branch, &fetched.branch),
            (&mut self.state, &fetched.state),
            (&mut self.updated_at, &fetched.updated_at),
        ] {
            if value.is_some() {
                *field = value.clone();
            }
        }
        *self != before
    }

    /// A link set through `imacs_plan_set` or `imacs_ticket_link`. Linking the same
    /// ticket again keeps what IMACS knows about it; a different ticket starts fresh.
    pub fn relink(new: Option<TicketLink>, old: Option<&TicketLink>) -> Option<TicketLink> {
        let Some(mut new) = new else { return old.cloned() };
        new.id = new.id.trim().to_string();
        match old.filter(|old| old.system == new.system || new.system.is_empty()).filter(|old| old.id == new.id) {
            Some(old) => {
                if new.system.is_empty() {
                    new.system = old.system.clone();
                }
                for (field, value) in [
                    (&mut new.key, &old.key),
                    (&mut new.url, &old.url),
                    (&mut new.title, &old.title),
                    (&mut new.branch, &old.branch),
                    (&mut new.state, &old.state),
                    (&mut new.updated_at, &old.updated_at),
                ] {
                    if field.is_none() {
                        *field = value.clone();
                    }
                }
                new.synced_at = old.synced_at;
                new.pushed_state = old.pushed_state.clone();
                new.error = old.error.clone();
            }
            None => {
                new.synced_at = None;
                new.pushed_state = None;
                new.error = None;
            }
        }
        Some(new)
    }
}

/// A ticket for `imacs_ticket_link` to create with the configured create operation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketDraft {
    pub title: String,
    #[serde(default)]
    pub body: String,
}

/// How the external system's work maps onto IMACS plans.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TicketMode {
    /// An external board maps to a plan, and its tickets to the plan's tasks.
    #[default]
    Board,
    /// One ticket maps to one plan.
    Ticket,
}

/// Who posts comments on tickets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommentPolicy {
    /// Agents' reports and the chief's lifecycle notes.
    #[default]
    Agents,
    /// Only the chief's lifecycle notes and explicit comments.
    Chief,
    /// No comments.
    Off,
}

/// The `[tickets]` section of a workspace file or the main config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TicketConfig {
    /// The ticket system's name, shown on cards and to the chief. Empty turns tickets off.
    pub system: String,
    pub mode: TicketMode,
    pub comments: CommentPolicy,
    /// Free text added to the chief's prompt: how this team works with its tickets.
    pub workflow: String,
    /// Template for newly created branches of linked work.
    pub branch: Option<String>,
    /// Longest wait for one MCP request.
    pub timeout_secs: u64,
    pub server: McpServer,
    /// The system's state names for IMACS's task statuses. Unmapped statuses are not pushed.
    pub states: StateMap,
    pub operations: Operations,
}

impl Default for TicketConfig {
    fn default() -> Self {
        Self {
            system: String::new(),
            mode: TicketMode::Board,
            comments: CommentPolicy::Agents,
            workflow: String::new(),
            branch: None,
            timeout_secs: 30,
            server: McpServer::default(),
            states: StateMap::default(),
            operations: Operations::default(),
        }
    }
}

/// An MCP server: a stdio command, or a streamable HTTP endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServer {
    /// Program and arguments of a stdio server. Takes precedence over `url`.
    pub command: Vec<String>,
    /// Extra environment for the stdio server.
    pub env: BTreeMap<String, String>,
    /// Streamable HTTP endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Environment variable holding a bearer token for `url`. Trimmed when the config is
    /// read (blank means none), so every reader looks up the same name.
    #[serde(skip_serializing_if = "Option::is_none", deserialize_with = "variable_name")]
    pub token_env: Option<String>,
}

impl McpServer {
    /// The variable a connection to this server authenticates with: `token_env`, for an
    /// HTTP server. A stdio server gets no token from IMACS.
    pub fn bearer_env(&self) -> Option<&str> {
        let http = self.command.is_empty() && self.url.as_deref().is_some_and(|url| !url.trim().is_empty());
        self.token_env.as_deref().filter(|_| http)
    }
}

fn variable_name<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    let name: Option<String> = Option::deserialize(deserializer)?;
    Ok(name.map(|name| name.trim().to_string()).filter(|name| !name.is_empty()))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StateMap {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub planned: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
}

impl StateMap {
    pub fn get(&self, status: TaskStatus) -> Option<&str> {
        match status {
            TaskStatus::Planned => &self.planned,
            TaskStatus::Running => &self.running,
            TaskStatus::Review => &self.review,
            TaskStatus::Done => &self.done,
            TaskStatus::Blocked => &self.blocked,
        }
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    }

    /// The IMACS status a system state name stands for, ignoring case.
    pub fn status_of(&self, state: &str) -> Option<TaskStatus> {
        TaskStatus::ALL
            .into_iter()
            .find(|status| self.get(*status).is_some_and(|s| s.eq_ignore_ascii_case(state.trim())))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Operations {
    /// Read one ticket. Placeholders: {id}, {key}, {system}.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub get: Option<Operation>,
    /// The tickets on a board, for board mode. Placeholders: {board}, {board_key}.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list: Option<Operation>,
    /// Placeholders: {id}, {key}, {state}.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_state: Option<Operation>,
    /// Placeholders: {id}, {key}, {body}.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<Operation>,
    /// Optional. Placeholders: {title}, {body}, {board}, {board_key}.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub create: Option<Operation>,
}

/// One generic operation: which tool to call, its arguments, and where its result
/// keeps the ticket's fields.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Operation {
    pub tool: String,
    /// Tool arguments. Strings may contain placeholders such as {id}.
    pub arguments: Map<String, Value>,
    /// For `list`: the JSON path of the ticket array in the result.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub items: String,
    /// JSON paths of the link fields in the result (in each item for `list`).
    pub result: FieldPaths,
}

/// Dotted JSON paths (`state.name`, `items.0.id`); an empty path leaves the field unread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FieldPaths {
    pub id: String,
    pub key: String,
    pub url: String,
    pub title: String,
    pub state: String,
    pub updated_at: String,
    pub branch: String,
}

impl Default for FieldPaths {
    fn default() -> Self {
        Self {
            id: "id".into(),
            key: "key".into(),
            url: "url".into(),
            title: "title".into(),
            state: "state".into(),
            updated_at: "updated_at".into(),
            branch: String::new(),
        }
    }
}

/// The longest ticket error kept: enough to diagnose, bounded for plan files and replies.
pub const MAX_ERROR_CHARS: usize = 600;

/// Placeholders operation arguments may use; other text in braces is left as written.
pub const PLACEHOLDERS: &[&str] = &["id", "key", "state", "body", "title", "board", "board_key", "system"];

/// Values for an operation's placeholders. A placeholder whose value is unknown fails
/// the operation with a message naming it, rather than sending an empty argument.
#[derive(Debug, Clone, Default)]
pub struct Vars {
    values: Vec<(&'static str, Option<String>)>,
}

impl Vars {
    pub fn new(system: &str) -> Vars {
        Vars::default().with("system", Some(system))
    }

    pub fn with(mut self, name: &'static str, value: Option<&str>) -> Vars {
        self.values.retain(|(n, _)| *n != name);
        self.values.push((name, value.map(str::to_string)));
        self
    }

    pub fn ticket(self, link: &TicketLink) -> Vars {
        self.with("id", Some(&link.id)).with("key", Some(link.label()))
    }

    pub fn board(self, link: Option<&TicketLink>) -> Vars {
        self.with("board", link.map(|l| l.id.as_str())).with("board_key", link.map(TicketLink::label))
    }

    /// Fills placeholders in one pass over the template, so text a value brings in,
    /// such as a brief mentioning `{title}`, is never substituted again.
    fn render(&self, text: &str) -> Result<String, String> {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find('{') {
            out.push_str(&rest[..start]);
            let after = &rest[start + 1..];
            let name = after.find('}').map(|end| &after[..end]).filter(|name| PLACEHOLDERS.contains(name));
            match name {
                Some(name) => {
                    match self.values.iter().find(|(n, _)| *n == name).and_then(|(_, v)| v.as_ref()) {
                        Some(value) => out.push_str(value),
                        None => return Err(format!("needs {{{name}}}, which is unknown here")),
                    }
                    rest = &after[name.len() + 1..];
                }
                None => {
                    out.push('{');
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        Ok(out)
    }
}

impl TicketConfig {
    /// Render only known placeholders in one pass. Missing values leave naming to IMACS.
    pub fn branch_name(&self, link: &TicketLink) -> Option<String> {
        if !self.accepts(link) {
            return None;
        }
        let template = self.branch.as_deref()?;
        let key = link.label();
        let lower = key.to_lowercase();
        let number = key.rsplit('-').next().filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
        let slug = link.title.as_deref().map(crate::plan::short_slug).filter(|s| !s.is_empty());
        let values = [
            ("key", Some(key).filter(|s| !s.is_empty())),
            ("key_lower", Some(lower.as_str()).filter(|s| !s.is_empty())),
            ("key_number", number),
            ("slug", slug.as_deref()),
            ("branch", link.branch.as_deref().filter(|s| !s.is_empty())),
        ];
        let mut rest = template;
        let mut out = String::new();
        while let Some(start) = rest.find('{') {
            out.push_str(&rest[..start]);
            let end = rest[start..].find('}')? + start;
            let name = &rest[start + 1..end];
            out.push_str(values.iter().find(|(n, _)| *n == name)?.1?);
            rest = &rest[end + 1..];
        }
        out.push_str(rest);
        (!out.is_empty()).then_some(out)
    }

    /// Configured with a system name and a server to reach it.
    pub fn is_configured(&self) -> bool {
        !self.system.trim().is_empty()
            && (!self.server.command.is_empty() || self.server.url.as_deref().is_some_and(|u| !u.trim().is_empty()))
    }

    /// Whether operations may address this link: it was made for this system, or for
    /// no system in particular. Links of another system are left alone after a switch,
    /// so their ids, briefs and reports never reach unrelated tickets.
    pub fn accepts(&self, link: &TicketLink) -> bool {
        link.system.is_empty() || link.system == self.system
    }

    /// Why a link is left alone, for the card and for replies.
    pub fn mismatch(&self, link: &TicketLink) -> Option<String> {
        (!self.accepts(link)).then(|| {
            format!("{} belongs to {}, but [tickets] is set up for {}", link.label(), link.system, self.system)
        })
    }

    pub fn operation(&self, name: &str) -> Option<&Operation> {
        match name {
            "get" => self.operations.get.as_ref(),
            "list" => self.operations.list.as_ref(),
            "update_state" => self.operations.update_state.as_ref(),
            "comment" => self.operations.comment.as_ref(),
            "create" => self.operations.create.as_ref(),
            _ => None,
        }
        .filter(|op| !op.tool.trim().is_empty())
    }

    /// Whether comments by this author are posted.
    pub fn posts(&self, author: &Author) -> bool {
        match (self.comments, author) {
            (CommentPolicy::Off, _) => false,
            (CommentPolicy::Agents, _) => true,
            (CommentPolicy::Chief, author) => !matches!(author, Author::Agent(_)),
        }
    }

    /// The chief's prompt section describing this workspace's ticket workflow.
    pub fn prompt_section(&self) -> String {
        let system = self.system.trim();
        let mut text = format!("Tickets: this workspace tracks its work in {system}, which IMACS reaches over MCP.\n");
        text.push_str(match self.mode {
            TicketMode::Board => {
                "Mode board: a board created in the ticket system maps to one plan, and each of its tickets to one task. \
                 The ticket says what to do; your brief says how. Give the plan the board's link (ticket in \
                 imacs_plan_set, or imacs_ticket_link) and every task its ticket (tasks[].ticket.id). imacs_ticket_sync \
                 lists the board's tickets and shows which have no task yet.\n"
            }
            TicketMode::Ticket => {
                "Mode ticket: one ticket maps to one plan. Give the plan that ticket (ticket in imacs_plan_set, or \
                 imacs_ticket_link); the plan's progress is reported on it.\n"
            }
        });
        let states: Vec<String> = TaskStatus::ALL
            .into_iter()
            .filter_map(|status| self.states.get(status).map(|state| format!("{} → {state}", status.label())))
            .collect();
        if !states.is_empty() {
            text.push_str(&format!(
                "IMACS sets ticket states as tasks move ({}), and shows each ticket's live state on its card.\n",
                states.join(", ")
            ));
        }
        text.push_str(match self.comments {
            CommentPolicy::Agents => {
                "IMACS comments on tickets for traceability: dispatches with your brief, agents' reports signed with \
                 their model, acceptances, blocks and completion. Add implementation details and decisions with \
                 imacs_ticket_comment.\n"
            }
            CommentPolicy::Chief => {
                "IMACS comments on tickets for your dispatches, acceptances, blocks and completion; agents' reports \
                 stay in IMACS. Add implementation details and decisions with imacs_ticket_comment.\n"
            }
            CommentPolicy::Off => "Ticket comments are off: do not comment on tickets.\n",
        });
        text.push_str(
            "Someone changing a ticket arrives as a ticket event in imacs_wait (include ticket in statuses): decide \
             whether the plan should follow it. Ticket failures show on the card and never block the plan.",
        );
        if !self.workflow.trim().is_empty() {
            text.push_str(&format!("\nWorkflow: {}", self.workflow.trim()));
        }
        text
    }
}

impl Operation {
    /// The tool arguments with placeholders filled in.
    pub fn arguments(&self, vars: &Vars) -> Result<Value, String> {
        fn fill(value: &Value, vars: &Vars) -> Result<Value, String> {
            Ok(match value {
                Value::String(text) => Value::String(vars.render(text)?),
                Value::Array(items) => Value::Array(items.iter().map(|v| fill(v, vars)).collect::<Result<_, _>>()?),
                Value::Object(map) => Value::Object(
                    map.iter().map(|(k, v)| Ok((k.clone(), fill(v, vars)?))).collect::<Result<_, String>>()?,
                ),
                other => other.clone(),
            })
        }
        fill(&Value::Object(self.arguments.clone()), vars).map_err(|err| format!("{}: {err}", self.tool))
    }

    /// The ticket a tool result describes. Fails when none of the paths match, which
    /// almost always means the result paths need adjusting.
    pub fn ticket(&self, result: &Value, system: &str) -> Result<TicketLink, String> {
        let link = self.result.read(result, system);
        if link == (TicketLink { system: system.to_string(), ..TicketLink::default() }) {
            return Err(format!(
                "{} returned no ticket fields at the configured result paths: {}",
                self.tool,
                snippet(result)
            ));
        }
        Ok(link)
    }

    /// The tickets a list result holds, at `items`.
    pub fn tickets(&self, result: &Value, system: &str) -> Result<Vec<TicketLink>, String> {
        match json_path(result, &self.items) {
            Some(Value::Array(items)) => Ok(items
                .iter()
                .map(|item| self.result.read(item, system))
                .filter(|link| !link.id.is_empty() || link.key.is_some())
                .collect()),
            _ => Err(format!("{} returned no ticket list at {:?}: {}", self.tool, self.items, snippet(result))),
        }
    }
}

impl FieldPaths {
    fn read(&self, value: &Value, system: &str) -> TicketLink {
        let field = |path: &str| {
            if path.trim().is_empty() { None } else { json_path(value, path).and_then(scalar) }
        };
        TicketLink {
            system: system.to_string(),
            id: field(&self.id).unwrap_or_default(),
            key: field(&self.key),
            url: field(&self.url),
            title: field(&self.title),
            branch: field(&self.branch),
            state: field(&self.state),
            updated_at: field(&self.updated_at),
            ..TicketLink::default()
        }
    }
}

/// The credentials a config runs with, which nothing from its server may carry into plans
/// or replies: the `token_env` value and secret-looking `server.env` values.
pub fn secrets_of(config: &TicketConfig, env: impl FnMut(&str) -> Option<String>) -> Vec<String> {
    let secret_name = |name: &str| {
        let name = name.to_ascii_uppercase();
        ["TOKEN", "KEY", "SECRET", "PASSWORD", "AUTH", "CREDENTIAL"].iter().any(|s| name.contains(s))
    };
    let token = config.server.token_env.as_deref().map(str::trim).filter(|name| !name.is_empty()).and_then(env);
    let values = config.server.env.iter().filter(|(name, _)| secret_name(name)).map(|(_, value)| value.clone());
    token.into_iter().chain(values).collect()
}

/// What replaces an error whose credential could not be masked in place.
pub const WITHHELD: &str = "the ticket server's message was withheld because it contained a credential";

/// What replaces a ticket field or JSON string whose credential could not be masked in place.
pub const WITHHELD_FIELD: &str = "[withheld: contained a credential]";

/// The longest ticket field kept.
pub const MAX_FIELD_CHARS: usize = 2000;

/// The one cleaner for every string that comes from a ticket server, a webhook or a caller
/// and is then stored or shown, built from the secrets of the job or config involved.
///
/// A known secret is masked in place wherever it can be found: raw, percent-encoded,
/// JSON- or `\u`-escaped, in any mix, even split by whitespace or line breaks. When a
/// secret still shows once the text is percent-decoded, unescaped and stripped of
/// whitespace, the whole field is withheld instead. Bounds cut only after cleaning, at a
/// word boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cleaner {
    secrets: Vec<String>,
}

impl Cleaner {
    /// Secrets shorter than six characters are ignored: masking them would mask words.
    pub fn new(secrets: impl IntoIterator<Item = String>) -> Cleaner {
        let mut kept: Vec<String> = Vec::new();
        for secret in secrets {
            let secret = secret.trim().to_string();
            if secret.chars().count() >= 6 && !kept.contains(&secret) {
                kept.push(secret);
            }
        }
        Cleaner { secrets: kept }
    }

    /// The cleaner for a config's credentials.
    pub fn of(config: &TicketConfig, env: impl FnMut(&str) -> Option<String>) -> Cleaner {
        Cleaner::new(secrets_of(config, env))
    }

    /// This cleaner, also covering `more`.
    pub fn with(self, more: impl IntoIterator<Item = String>) -> Cleaner {
        Cleaner::new(self.secrets.into_iter().chain(more))
    }

    pub fn secrets(&self) -> &[String] {
        &self.secrets
    }

    /// A ticket field shown on a card, or a webhook field: cleaned, and bounded for display.
    pub fn field(&self, text: &str) -> String {
        bound(&self.text(text), MAX_FIELD_CHARS)
    }

    /// Text cleaned without a bound, for payloads such as agent reads that need it whole.
    pub fn text(&self, text: &str) -> String {
        self.masked(text).unwrap_or_else(|| WITHHELD_FIELD.into())
    }

    /// An error message: field rules, then bearer and basic credentials, `Authorization`
    /// values, URL user info, secret-looking query values and long credential-like runs.
    pub fn error(&self, text: &str) -> String {
        let Some(masked) = self.masked(text) else { return WITHHELD.into() };
        let out = redact_runs(&redact_query(&redact_userinfo(&redact_credentials(&masked)), secret_param));
        bound(&out, MAX_ERROR_CHARS)
    }

    /// A URL to store and open: without user info, credential parameters or parameters
    /// whose value holds a secret, and field rules for the rest.
    pub fn url(&self, url: &str) -> String {
        self.field(&strip_url(url, &|value| self.holds_secret(value)))
    }

    /// A ticket as read or given: every field cleaned.
    pub fn link(&self, link: TicketLink) -> TicketLink {
        let field = |text: Option<String>| text.map(|text| self.field(&text));
        TicketLink {
            id: self.field(&link.id),
            key: field(link.key),
            url: link.url.map(|url| self.url(&url)),
            title: field(link.title),
            branch: field(link.branch),
            state: field(link.state),
            updated_at: field(link.updated_at),
            pushed_state: field(link.pushed_state),
            error: link.error.map(|error| self.error(&error)),
            ..link
        }
    }

    /// A ticket read for an agent: every string and property name cleaned, with URL user
    /// info and credential query values in its text masked in place. Strings stay whole: the
    /// card bound is for display, and an agent needs the end of a long description too.
    pub fn value(&self, value: Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.text(&redact_query(&redact_userinfo(&text), credential_param))),
            Value::Array(items) => Value::Array(items.into_iter().map(|item| self.value(item)).collect()),
            Value::Object(map) => {
                Value::Object(map.into_iter().map(|(key, item)| (self.text(&key), self.value(item))).collect())
            }
            other => other,
        }
    }

    /// The text with known secrets masked, or `None` when one survives masking.
    fn masked(&self, text: &str) -> Option<String> {
        let masked = self.secrets.iter().fold(text.to_string(), |text, secret| mask(&text, secret));
        (!self.holds_secret(&masked)).then_some(masked)
    }

    /// Whether a known secret shows once percent-encoding and JSON escapes are undone and
    /// whitespace is removed.
    fn holds_secret(&self, text: &str) -> bool {
        let normalized = normalize(text);
        self.secrets.iter().any(|secret| normalized.contains(&normalize(secret)))
    }
}

/// Text cut to `max` characters, at the last word boundary before it, after cleaning.
fn bound(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_string(),
        Some((cut, _)) => {
            let cut = text[..cut].rfind(char::is_whitespace).filter(|&at| at > 0).unwrap_or(cut);
            format!("{}…", text[..cut].trim_end())
        }
    }
}

/// Percent-decoded and JSON-unescaped (repeatedly, so nested encodings unfold), without whitespace.
fn normalize(text: &str) -> String {
    let mut current = text.to_string();
    for _ in 0..3 {
        let next = unescape_json(&percent_decode(&current));
        if next == current {
            break;
        }
        current = next;
    }
    current.chars().filter(|c| !c.is_whitespace()).collect()
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let decoded = (bytes[i] == b'%')
            .then(|| bytes.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match decoded {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn unescape_json(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        let escape = &rest[at + 1..];
        let (decoded, used) = match escape.chars().next() {
            Some('n') => (Some('\n'), 1),
            Some('r') => (Some('\r'), 1),
            Some('t') => (Some('\t'), 1),
            Some('b') => (Some('\u{8}'), 1),
            Some('f') => (Some('\u{c}'), 1),
            Some(c @ ('/' | '\\' | '"')) => (Some(c), 1),
            Some('u') => {
                (escape.get(1..5).and_then(|hex| u32::from_str_radix(hex, 16).ok()).and_then(char::from_u32), 5)
            }
            _ => (None, 0),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &escape[used..];
            }
            None => {
                out.push('\\');
                rest = escape;
            }
        }
    }
    out.push_str(rest);
    out
}

/// A URL without user info and without secret-looking query or fragment parameters, so
/// the open action and stored plans never carry a credential.
fn strip_url(url: &str, holds_secret: &dyn Fn(&str) -> bool) -> String {
    let Some(scheme) = url.find("://") else { return url.to_string() };
    let start = scheme + 3;
    let authority_end = url[start..].find(['/', '?', '#']).map_or(url.len(), |end| start + end);
    let host = url[start..authority_end].rfind('@').map_or(start, |at| start + at + 1);
    let mut out = format!("{}{}", &url[..start], &url[host..authority_end]);
    let rest = &url[authority_end..];
    let (rest, fragment) = rest.split_once('#').map_or((rest, None), |(rest, fragment)| (rest, Some(fragment)));
    let (path, query) = rest.split_once('?').map_or((rest, None), |(path, query)| (path, Some(query)));
    out.push_str(path);
    let public = |params: &str| {
        params
            .split('&')
            .filter(|param| {
                let (name, value) = param.split_once('=').unwrap_or((param, ""));
                !param.is_empty() && !credential_param(name) && !holds_secret(value)
            })
            .collect::<Vec<_>>()
            .join("&")
    };
    if let Some(query) = query.map(public).filter(|query| !query.is_empty()) {
        out.push('?');
        out.push_str(&query);
    }
    match fragment {
        Some(fragment) if fragment.contains('=') => {
            let fragment = public(fragment);
            if !fragment.is_empty() {
                out.push('#');
                out.push_str(&fragment);
            }
        }
        Some(fragment) => {
            out.push('#');
            out.push_str(fragment);
        }
        None => {}
    }
    out
}

/// URL parameters that carry credentials by name, compared decoded and ignoring case. A
/// name only containing such a word (issueKey, assignee) is a public parameter.
const CREDENTIAL_PARAMS: &[&str] = &[
    "access_token",
    "token",
    "api_key",
    "apikey",
    "password",
    "passwd",
    "secret",
    "client_secret",
    "refresh_token",
    "id_token",
    "auth",
    "authorization",
    "sig",
    "signature",
    "x-amz-signature",
    "x-amz-credential",
    "x-amz-security-token",
];

fn credential_param(name: &str) -> bool {
    let name = percent_decode(name).to_ascii_lowercase();
    CREDENTIAL_PARAMS.contains(&name.as_str())
}

/// Parameter names that may carry credentials, for error text, where masking a public
/// value costs nothing.
fn secret_param(name: &str) -> bool {
    const SECRET: &[&str] =
        &["token", "key", "secret", "password", "passwd", "pwd", "auth", "sig", "credential", "session", "code"];
    let name = name.to_ascii_lowercase();
    SECRET.iter().any(|secret| name.contains(secret))
}

/// Every occurrence of a known secret, in any encoding, becomes `***`.
fn mask(text: &str, secret: &str) -> String {
    let mut masked = String::with_capacity(text.len());
    let mut pos = 0;
    while let Some((start, end)) = find_secret(text, secret, pos) {
        masked.push_str(&text[pos..start]);
        masked.push_str("***");
        pos = end;
    }
    masked.push_str(&text[pos..]);
    masked
}

/// Where `secret` first occurs in `text` from byte `from`, with each of its characters
/// written raw, percent-encoded, JSON-escaped (`\/`, `\\`, `\"`) or `\u`-escaped, in
/// any mix, as JSON bodies and URLs echo them.
fn find_secret(text: &str, secret: &str, from: usize) -> Option<(usize, usize)> {
    (from..text.len()).filter(|&start| text.is_char_boundary(start)).find_map(|start| {
        let mut chars = secret.chars();
        let first = encoded_char(text, start, chars.next()?)?;
        chars.try_fold(first, |at, c| encoded_char(text, skip_gap(text, at), c)).map(|end| (start, end))
    })
}

/// Skips whitespace inside a secret: raw, escaped (`\n`, `\r`, `\t`) or percent-encoded.
fn skip_gap(text: &str, mut at: usize) -> usize {
    loop {
        let rest = &text[at..];
        let step = if rest.starts_with(char::is_whitespace) {
            rest.chars().next().map_or(0, char::len_utf8)
        } else if ["\\n", "\\r", "\\t"].iter().any(|escape| rest.starts_with(escape)) {
            2
        } else if ["%0a", "%0d", "%09", "%20"]
            .iter()
            .any(|code| rest.get(..3).is_some_and(|r| r.eq_ignore_ascii_case(code)))
        {
            3
        } else {
            0
        };
        if step == 0 {
            return at;
        }
        at += step;
    }
}

/// Where `c` ends when it is written at byte `at` of `text` in one of its encodings.
fn encoded_char(text: &str, at: usize, c: char) -> Option<usize> {
    let bytes = text.as_bytes();
    let rest = &bytes[at..];
    let mut raw = [0u8; 4];
    let raw = c.encode_utf8(&mut raw).as_bytes();
    if rest.starts_with(raw) {
        return Some(at + raw.len());
    }
    let hex = |digits: &[u8]| std::str::from_utf8(digits).ok().and_then(|d| u32::from_str_radix(d, 16).ok());
    if rest.len() >= 6 && rest.starts_with(b"\\u") && hex(&rest[2..6]) == Some(c as u32) {
        return Some(at + 6);
    }
    if matches!(c, '/' | '\\' | '"') && rest.len() >= 2 && rest[0] == b'\\' && rest[1] == c as u8 {
        return Some(at + 2);
    }
    let mut end = at;
    for byte in raw {
        let encoded = bytes.get(end..end + 3)?;
        if encoded[0] != b'%' || hex(&encoded[1..]) != Some(u32::from(*byte)) {
            return None;
        }
        end += 3;
    }
    Some(end)
}

/// Long runs of credential characters mixing letters and digits, like the tokens of most
/// services, become `***` even when IMACS was never told the secret.
fn redact_runs(text: &str) -> String {
    let credential = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '%' | '.' | '-' | '\\');
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        let looks_like_credential = run.chars().count() >= 24
            && run.chars().any(|c| c.is_ascii_digit())
            && run.chars().any(|c| c.is_ascii_alphabetic());
        out.push_str(if looks_like_credential { "***" } else { run });
        run.clear();
    };
    for c in text.chars() {
        if credential(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

fn value_end(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ';' | '}' | ')' | ']' | '<' | '>' | '&' | '#')
}

/// Skips a value starting at `from` and returns where it ends.
fn skip_value(text: &str, from: usize) -> usize {
    text[from..].find(value_end).map_or(text.len(), |end| from + end)
}

fn redact_credentials(text: &str) -> String {
    // ASCII lowercasing keeps byte offsets, so positions in `lower` index `text` too.
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let rest = &lower[i..];
        if let Some(scheme) = ["bearer", "basic"].into_iter().find(|s| rest.starts_with(s)) {
            let spaces = text[i + scheme.len()..].len() - text[i + scheme.len()..].trim_start().len();
            if spaces > 0 && (i == 0 || !lower[..i].ends_with(|c: char| c.is_ascii_alphanumeric())) {
                let start = i + scheme.len() + spaces;
                let end = skip_value(text, start);
                // Prose ("a bearer of news") stays; credentials are long or not just letters.
                let word = &text[start..end];
                if word.chars().count() >= 8 || word.chars().any(|c| !c.is_alphabetic()) {
                    out.push_str(&text[i..start]);
                    out.push_str("***");
                    i = end;
                    continue;
                }
            }
        }
        if rest.starts_with("authorization") {
            let key_end = i + "authorization".len();
            let gap = text[key_end..]
                .char_indices()
                .find(|(_, c)| !matches!(c, '"' | '\'' | ' ' | ':' | '='))
                .map_or(text.len() - key_end, |(n, _)| n);
            let separated = text[key_end..key_end + gap].contains([':', '=']);
            let value = key_end + gap;
            let scheme = ["bearer", "basic"].iter().any(|s| lower[value..].starts_with(s));
            if separated && !scheme {
                let end = skip_value(text, value);
                out.push_str(&text[i..value]);
                if end > value {
                    out.push_str("***");
                }
                i = end;
                continue;
            }
        }
        let c = text[i..].chars().next().expect("in bounds");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// `scheme://user:password@host` keeps only the host.
fn redact_userinfo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let start = at + 3;
        out.push_str(&rest[..start]);
        let authority_end =
            rest[start..].find(|c: char| value_end(c) || matches!(c, '/' | '?')).map_or(rest.len(), |end| start + end);
        match rest[start..authority_end].rfind('@') {
            Some(user) => {
                out.push_str("***");
                out.push_str(&rest[start + user..authority_end]);
            }
            None => out.push_str(&rest[start..authority_end]),
        }
        rest = &rest[authority_end..];
    }
    out.push_str(rest);
    out
}

/// `?access_token=…` and `&api_key=…` and the like keep their names only.
fn redact_query(text: &str, secret: fn(&str) -> bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(found) = text[i..].find(['?', '&']) {
        let name_start = i + found + 1;
        out.push_str(&text[i..name_start]);
        let name_end = text[name_start..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '[' | ']')))
            .map_or(text.len(), |end| name_start + end);
        if text[name_end..].starts_with('=') && secret(&text[name_start..name_end]) {
            out.push_str(&text[name_start..=name_end]);
            out.push_str("***");
            i = skip_value(text, name_end + 1);
        } else {
            i = name_start;
        }
    }
    out.push_str(&text[i..]);
    out
}

/// Follows a simple JSON path: dot-separated keys and array indexes, with an optional
/// leading `$`, and `[n]` as another spelling of `.n`. An empty path is the value itself.
pub fn json_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let path = path.trim();
    let path = path.strip_prefix('$').unwrap_or(path).replace('[', ".").replace(']', "");
    path.split('.').filter(|segment| !segment.is_empty()).try_fold(value, |current, segment| match current {
        Value::Object(map) => map.get(segment),
        Value::Array(items) => items.get(segment.parse::<usize>().ok()?),
        _ => None,
    })
}

/// A field's text: strings as they are, other scalars printed, objects compactly, so a
/// path that stops one level too early is visible on the card instead of silently empty.
fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    }
}

fn snippet(value: &Value) -> String {
    let text = value.to_string();
    match text.char_indices().nth(240) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

/// What JSON paths are read from in an MCP `tools/call` result: its structured
/// content, else its text content parsed as JSON, else that text as a string.
pub fn tool_output(result: &Value) -> Value {
    if let Some(structured) = result.get("structuredContent").filter(|v| !v.is_null()) {
        return structured.clone();
    }
    let texts: Vec<&str> = result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect();
    let joined = texts.join("\n");
    serde_json::from_str(&joined)
        .ok()
        .or_else(|| texts.iter().find_map(|text| serde_json::from_str(text).ok()))
        .unwrap_or(Value::String(joined))
}

/// The status a plan's one ticket follows in ticket mode.
pub fn plan_status(plan: &Plan) -> TaskStatus {
    let any = |status| plan.tasks.iter().any(|t| t.status == status);
    if plan.status == PlanStatus::Completed
        || (!plan.tasks.is_empty() && plan.tasks.iter().all(|t| t.status == TaskStatus::Done))
    {
        TaskStatus::Done
    } else if any(TaskStatus::Blocked) {
        TaskStatus::Blocked
    } else if any(TaskStatus::Running) {
        TaskStatus::Running
    } else if any(TaskStatus::Review)
        && plan.tasks.iter().all(|t| matches!(t.status, TaskStatus::Review | TaskStatus::Done))
    {
        TaskStatus::Review
    } else if any(TaskStatus::Review) || any(TaskStatus::Done) {
        TaskStatus::Running
    } else {
        TaskStatus::Planned
    }
}

/// Who a ticket comment comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Author {
    /// An agent reporting on its task, by its model label.
    Agent(String),
    /// The chief, by its model label.
    Chief(String),
    /// The user, acting on the IMACS board.
    User,
    /// IMACS itself, noticing something such as an agent exiting.
    Imacs,
}

impl Author {
    fn opening(&self, task: Option<&Task>) -> String {
        let task_line = |task: &Task| format!("task {} ({})", task.id, task.title.trim());
        match (self, task) {
            (Author::Agent(model), Some(task)) => format!("This is {model}, the IMACS agent on {}.", task_line(task)),
            (Author::Agent(model), None) => format!("This is {model}, an IMACS agent."),
            (Author::Chief(model), _) => format!("This is {model}, the IMACS chief."),
            (Author::User, _) => "Posted by IMACS for the user.".into(),
            (Author::Imacs, _) => "Posted by IMACS.".into(),
        }
    }
}

/// The comment for a task being dispatched: the implementation brief, so the ticket
/// carries how the work is done next to what it asks for.
pub fn dispatch_comment(chief: &Author, task: &Task, agent: &str) -> String {
    let mut text = format!(
        "{}\nDispatched task {} ({}) to {agent}{}.\n\nImplementation brief:\n{}",
        chief.opening(None),
        task.id,
        task.title.trim(),
        task.branch.as_deref().map(|b| format!(" on branch {b}")).unwrap_or_default(),
        task.brief.trim()
    );
    if !task.acceptance.is_empty() {
        text.push_str("\n\nDone when:");
        for check in &task.acceptance {
            text.push_str(&format!("\n- {}", check.trim()));
        }
    }
    text
}

/// An agent's `imacs_report`, signed with its model.
pub fn report_comment(agent: &Author, task: &Task, status: &str, summary: &str) -> String {
    format!("{}\nReported {status}:\n\n{}", agent.opening(Some(task)), summary.trim())
}

/// A task accepted as done, or blocked, with the note that came with it.
pub fn status_comment(author: &Author, task: &Task, status: TaskStatus, note: Option<&str>) -> String {
    let what = match status {
        TaskStatus::Done => "accepted",
        TaskStatus::Blocked => "is blocked",
        other => other.label(),
    };
    let mut text = format!("{}\nTask {} ({}) {what}.", author.opening(None), task.id, task.title.trim());
    if let Some(note) = note.map(str::trim).filter(|n| !n.is_empty()) {
        text.push_str(&format!("\n\n{note}"));
    }
    text
}

pub fn completed_comment(author: &Author, plan: &Plan) -> String {
    format!(
        "{}\nPlan complete: {} ({} of {} tasks done, integrated on {}).",
        author.opening(None),
        plan.goal.trim(),
        plan.count(TaskStatus::Done),
        plan.tasks.len(),
        plan.integration_branch()
    )
}

/// An explicit `imacs_ticket_comment`.
pub fn chief_comment(chief: &Author, text: &str) -> String {
    format!("{}\n\n{}", chief.opening(None), text.trim())
}

/// An explicit scoped agent comment, with the same task signature as reports.
pub fn agent_comment(agent: &Author, task: &Task, text: &str) -> String {
    format!("{}\n\n{}", agent.opening(Some(task)), text.trim())
}

/// A model label for signatures, such as "GPT-6.1 Sol (high)", from an agent's
/// command-line arguments; the agent's name when they name no model.
pub fn model_label(agent: &str, args: &[String]) -> String {
    let value = |names: &[&str]| {
        args.iter().enumerate().find_map(|(i, arg)| {
            names.iter().find_map(|name| {
                if arg == name {
                    args.get(i + 1).cloned()
                } else {
                    arg.strip_prefix(&format!("{name}=")).map(str::to_string)
                }
            })
        })
    };
    // Codex's `-c key=value` overrides.
    let setting = |key: &str| {
        args.windows(2).find_map(|pair| {
            (pair[0] == "-c" || pair[0] == "--config")
                .then(|| pair[1].strip_prefix(&format!("{key}=")))
                .flatten()
                .map(|v| v.trim_matches(|c| c == '"' || c == '\'').to_string())
        })
    };
    let model = value(&["-m", "--model"]).or_else(|| setting("model"));
    let effort = value(&["--effort", "--reasoning-effort"]).or_else(|| setting("model_reasoning_effort"));
    let name = match model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        Some(model) => pretty_model(model),
        None => {
            let agent = if agent.trim().is_empty() { "claude" } else { agent.trim() };
            let mut chars = agent.chars();
            chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
        }
    };
    match effort.map(|e| e.trim().to_string()).filter(|e| !e.is_empty()) {
        Some(effort) => format!("{name} ({effort})"),
        None => name,
    }
}

/// "gpt-6.1-sol" → "GPT-6.1 Sol", "claude-opus-5-5" → "Claude Opus 5.5".
pub fn pretty_model(model: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    for token in model.split(['-', '_', ' ']).filter(|t| !t.is_empty()) {
        let numeric = token.chars().all(|c| c.is_ascii_digit() || c == '.');
        if numeric && token.len() == 8 && token.chars().all(|c| c.is_ascii_digit()) {
            continue; // a release date
        }
        match words.last_mut() {
            Some(last) if numeric && last.ends_with(|c: char| c.is_ascii_digit()) => {
                last.push('.');
                last.push_str(token);
            }
            Some(last) if numeric && last == "GPT" => {
                last.push('-');
                last.push_str(token);
            }
            _ if token.eq_ignore_ascii_case("gpt") => words.push("GPT".into()),
            _ => {
                let mut chars = token.chars();
                words.push(chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default());
            }
        }
    }
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn redact(text: &str, secrets: &[String]) -> String {
        Cleaner::new(secrets.to_vec()).error(text)
    }

    fn clean_link(link: TicketLink, secrets: &[String]) -> TicketLink {
        Cleaner::new(secrets.to_vec()).link(link)
    }

    fn clean_value(value: Value, secrets: &[String]) -> Value {
        Cleaner::new(secrets.to_vec()).value(value)
    }

    fn link(id: &str) -> TicketLink {
        TicketLink { system: "alpha".into(), id: id.into(), ..TicketLink::default() }
    }

    #[test]
    fn branch_templates_require_values_and_render_once() {
        let mut config = TicketConfig {
            branch: Some("{key}/{key_lower}/{key_number}/{slug}/{branch}".into()),
            ..Default::default()
        };
        let link = TicketLink {
            id: "uuid".into(),
            key: Some("ENG-42".into()),
            title: Some("Fix parser now".into()),
            branch: Some("feature/parser".into()),
            ..Default::default()
        };
        assert_eq!(config.branch_name(&link).as_deref(), Some("ENG-42/eng-42/42/fix-parser-now/feature/parser"));
        assert!(config.branch_name(&TicketLink { title: None, ..link.clone() }).is_none());
        assert!(config.branch_name(&TicketLink { branch: None, ..link.clone() }).is_none());
        assert!(config.branch_name(&TicketLink { key: Some("ENG".into()), ..link.clone() }).is_none());
        config.branch = Some("{unknown}".into());
        assert!(config.branch_name(&link).is_none());
        config.branch = Some("{branch}".into());
        assert_eq!(config.branch_name(&TicketLink { branch: Some("{key}".into()), ..link }).as_deref(), Some("{key}"));
    }

    #[test]
    fn links_round_trip_on_plans_and_tasks_and_old_plans_load_without_them() {
        let ticket = TicketLink {
            key: Some("AL-7".into()),
            url: Some("https://tickets.example/AL-7".into()),
            title: Some("Ship it".into()),
            state: Some("Doing".into()),
            updated_at: Some("2026-10-29T10:00:00Z".into()),
            synced_at: Some(42),
            pushed_state: Some("Doing".into()),
            error: Some("comment failed".into()),
            ..link("7")
        };
        let plan = Plan {
            id: "p".into(),
            ticket: Some(link("board-1")),
            tasks: vec![Task { id: "a".into(), ticket: Some(ticket.clone()), ..Task::default() }, Task::default()],
            ..Plan::default()
        };
        let value = serde_json::to_value(&plan).unwrap();
        assert_eq!(value["ticket"], json!({"system":"alpha", "id":"board-1"}), "unknown fields stay out of the file");
        assert!(value["tasks"][1].get("ticket").is_none());
        assert_eq!(serde_json::from_value::<Plan>(value).unwrap(), plan);
        let old: Plan = serde_json::from_str(r#"{"goal":"g","tasks":[{"id":"a"}],"approved":false}"#).unwrap();
        assert!(old.ticket.is_none() && old.tasks[0].ticket.is_none());
        let minimal: TicketLink = serde_json::from_str(r#"{"id":"AL-1"}"#).unwrap();
        assert_eq!(minimal.label(), "AL-1");
        assert!(minimal.matches(Some("alpha"), None, Some("AL-1")), "an unnamed system matches any");
    }

    #[test]
    fn relinking_keeps_progress_for_the_same_ticket_and_replace_keeps_links() {
        let old = TicketLink {
            key: Some("AL-7".into()),
            state: Some("Doing".into()),
            synced_at: Some(5),
            pushed_state: Some("Doing".into()),
            error: Some("boom".into()),
            ..link("7")
        };
        let same =
            TicketLink::relink(Some(TicketLink { id: " 7 ".into(), ..TicketLink::default() }), Some(&old)).unwrap();
        assert_eq!(same, old);
        let other =
            TicketLink::relink(Some(TicketLink { pushed_state: Some("x".into()), ..link("8") }), Some(&old)).unwrap();
        assert_eq!(other, link("8"), "a new ticket never inherits pushed state or errors");
        assert_eq!(TicketLink::relink(None, Some(&old)), Some(old.clone()));

        let mut plan = Plan::default();
        plan.replace("g".into(), vec![Task { id: "a".into(), ticket: Some(old.clone()), ..Task::default() }]);
        let fresh = plan.tasks[0].ticket.clone().unwrap();
        assert_eq!((fresh.synced_at, fresh.pushed_state, fresh.error), (None, None, None), "new links start fresh");
        assert_eq!(fresh.state.as_deref(), Some("Doing"));
        plan.tasks[0].ticket = Some(old.clone());
        plan.replace("reworded".into(), vec![Task { id: "a".into(), ..Task::default() }]);
        assert_eq!(plan.tasks[0].ticket.as_ref(), Some(&old), "omitting a ticket keeps it");
        plan.replace("g".into(), vec![Task { id: "a".into(), ticket: Some(link("7")), ..Task::default() }]);
        assert_eq!(plan.tasks[0].ticket.as_ref().unwrap().pushed_state.as_deref(), Some("Doing"));
        assert!(plan.tasks.iter().any(|t| t.id == crate::plan::REVIEW_TASK && t.ticket.is_none()));
    }

    #[test]
    fn matching_uses_id_or_key_within_the_system() {
        let ticket = TicketLink { key: Some("AL-7".into()), ..link("7") };
        assert!(ticket.matches(None, Some("7"), None));
        assert!(ticket.matches(Some("alpha"), None, Some("AL-7")));
        assert!(ticket.matches(None, Some("AL-7"), None), "an id reference may name the key");
        assert!(!ticket.matches(Some("beta"), Some("7"), None));
        assert!(!ticket.matches(None, Some(""), Some("")));
        assert!(!TicketLink::default().matches(None, Some(""), None));
        let mut fetched = TicketLink { id: "uuid-7".into(), state: Some("Done".into()), ..ticket.clone() };
        assert!(ticket.same_ticket(&fetched));
        let mut updated = ticket.clone();
        assert!(updated.update_from(&fetched));
        assert_eq!(updated.id, "7", "the id operations address stays");
        assert_eq!(updated.state.as_deref(), Some("Done"));
        fetched.state = None;
        assert!(!updated.update_from(&fetched), "missing fields keep what is known");
    }

    #[test]
    fn links_of_another_system_are_not_accepted() {
        let config = TicketConfig { system: "alpha".into(), ..TicketConfig::default() };
        assert!(config.accepts(&link("1")));
        assert!(config.accepts(&TicketLink { system: String::new(), ..link("1") }), "a link made for no system");
        let foreign = TicketLink { system: "beta".into(), key: Some("B-1".into()), ..link("1") };
        assert!(!config.accepts(&foreign));
        assert_eq!(config.mismatch(&foreign).unwrap(), "B-1 belongs to beta, but [tickets] is set up for alpha");
        assert_eq!(config.mismatch(&link("1")), None);
    }

    #[test]
    fn json_paths_follow_keys_indexes_and_print_scalars() {
        let value = json!({"issue": {"state": {"name": "Doing"}, "n": 7, "ok": true, "labels": [{"name": "x"}]}});
        assert_eq!(json_path(&value, "issue.state.name"), Some(&json!("Doing")));
        assert_eq!(json_path(&value, "$.issue.labels[0].name"), Some(&json!("x")));
        assert_eq!(json_path(&value, "issue.labels.0.name"), Some(&json!("x")));
        assert_eq!(json_path(&value, ""), Some(&value));
        assert_eq!(json_path(&value, "$"), Some(&value));
        assert_eq!(json_path(&value, "issue.missing"), None);
        assert_eq!(json_path(&value, "issue.labels.x"), None);
        assert_eq!(scalar(&value["issue"]["n"]).as_deref(), Some("7"));
        assert_eq!(scalar(&value["issue"]["ok"]).as_deref(), Some("true"));
        assert_eq!(scalar(&value["issue"]["state"]).as_deref(), Some(r#"{"name":"Doing"}"#));
        assert_eq!(scalar(&Value::Null), None);
    }

    #[test]
    fn tool_output_prefers_structured_content_then_json_text() {
        let structured =
            json!({"structuredContent": {"id": "1"}, "content": [{"type":"text","text":"{\"id\":\"2\"}"}]});
        assert_eq!(tool_output(&structured), json!({"id": "1"}));
        let text = json!({"content": [{"type":"text","text":"{\"id\":\"2\"}"}]});
        assert_eq!(tool_output(&text), json!({"id": "2"}));
        let mixed = json!({"content": [{"type":"text","text":"Found it:"}, {"type":"text","text":"{\"id\":\"3\"}"}]});
        assert_eq!(tool_output(&mixed), json!({"id": "3"}));
        assert_eq!(tool_output(&json!({"content": [{"type":"text","text":"Saved"}]})), json!("Saved"));
    }

    fn config(text: &str) -> TicketConfig {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn the_operation_map_renders_arguments_and_maps_results() {
        let config = config(
            r#"
            system = "alpha"
            [server]
            command = ["alpha-mcp"]
            [operations.get]
            tool = "card_get"
            arguments = { card = "{id}", include = ["column", "{system}"], depth = 2, label = "key {key}" }
            result = { id = "card.ref", key = "card.code", url = "card.href", title = "card.name", state = "card.column.title", updated_at = "" }
            [operations.list]
            tool = "card_list"
            arguments = { board = "{board}" }
            items = "cards"
            result = { id = "ref", key = "code", state = "column.title" }
            [operations.comment]
            tool = "card_note"
            arguments = { card = "{id}", text = "{body}" }
            "#,
        );
        assert!(config.is_configured());
        let get = config.operation("get").unwrap();
        let ticket = TicketLink { key: Some("AL-7".into()), ..link("7") };
        assert_eq!(
            get.arguments(&Vars::new("alpha").ticket(&ticket)).unwrap(),
            json!({"card":"7", "include":["column","alpha"], "depth":2, "label":"key AL-7"})
        );
        let result = json!({"card": {"ref": "7", "code": "AL-7", "href": "https://x/7", "name": "Ship", "column": {"title": "Doing"}, "touched": "t1"}});
        let read = get.ticket(&result, "alpha").unwrap();
        assert_eq!(read.label(), "AL-7");
        assert_eq!(read.url.as_deref(), Some("https://x/7"));
        assert_eq!(read.state.as_deref(), Some("Doing"));
        assert_eq!(read.updated_at, None, "an empty path is not read");
        let error = get.ticket(&json!({"other": 1}), "alpha").unwrap_err();
        assert!(error.contains("card_get") && error.contains("other"), "{error}");

        let list = config.operation("list").unwrap();
        let error = list.arguments(&Vars::new("alpha").board(None)).unwrap_err();
        assert_eq!(error, "card_list: needs {board}, which is unknown here");
        assert_eq!(list.arguments(&Vars::new("alpha").board(Some(&link("b1")))).unwrap(), json!({"board":"b1"}));
        let items = list
            .tickets(
                &json!({"cards": [{"ref":"7","code":"AL-7","column":{"title":"Doing"}}, {"name":"no id"}]}),
                "alpha",
            )
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].state.as_deref(), Some("Doing"));
        assert!(list.tickets(&json!({"cards": "none"}), "alpha").unwrap_err().contains("no ticket list"));
        let comment = config.operation("comment").unwrap();
        assert!(comment.arguments(&Vars::new("alpha").ticket(&ticket)).unwrap_err().contains("{body}"));
        assert!(config.operation("update_state").is_none());
        assert!(!TicketConfig::default().is_configured());
    }

    #[test]
    fn redaction_finds_secrets_in_any_encoding_and_unknown_credentials() {
        let token = "sk/abc+def=ghi_123";
        let secrets = vec![token.to_string()];
        for leaked in [
            token,
            "sk%2Fabc%2Bdef%3Dghi_123",
            "sk%2fabc%2bdef%3dghi_123",
            "sk\\/abc+def=ghi_123",
            "sk\\u002fabc\\u002Bdef\\u003dghi_123",
            "sk\\/abc%2Bdef\\u003dghi_123",
        ] {
            assert_eq!(
                redact(&format!("said {leaked} twice: {leaked}."), &secrets),
                "said *** twice: ***.",
                "{leaked}"
            );
        }
        // Content blocks are joined with newlines, which can split a secret.
        assert_eq!(redact("tool failed: key sk/abc+de\nf=ghi_123 rejected", &secrets), "tool failed: key *** rejected");
        // Credentials IMACS was never told about: long runs of letters and digits.
        assert_eq!(redact("upstream key ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3 rejected", &[]), "upstream key *** rejected");
        assert_eq!(redact("token: dGhpcyBpcyBhIHNlY3JldCB0b2tlbiAxMjM0NTY3OA==", &[]), "token: ***");
        for kept in
            ["no ticket ENG-123 in /usr/local/lib/node_modules/mcp-remote", "rate limited until 2026-10-29T10:00:00Z"]
        {
            assert_eq!(redact(kept, &secrets), kept);
        }
    }

    #[test]
    fn tickets_read_from_a_server_lose_secrets_in_every_field() {
        let secrets = vec!["tok/en+42=".to_string()];
        let read = TicketLink {
            system: "alpha".into(),
            id: "7".into(),
            key: Some("AL-7".into()),
            title: Some("Rotate tok%2Fen%2B42%3D now".into()),
            state: Some("Doing".into()),
            url: Some("https://me:pw@t.example/7?view=full&access_token=x&API_KEY=y#code=z&tab=2".into()),
            updated_at: Some("2026-10-29".into()),
            ..TicketLink::default()
        };
        let clean = clean_link(read.clone(), &secrets);
        assert_eq!(clean.title.as_deref(), Some("Rotate *** now"));
        assert_eq!(clean.url.as_deref(), Some("https://t.example/7?view=full#code=z&tab=2"));
        assert_eq!(TicketLink { title: read.title.clone(), url: read.url.clone(), ..clean.clone() }, read);
        for (url, expected) in [
            ("https://t.example/7", "https://t.example/7"),
            ("https://t.example/7?token=1", "https://t.example/7"),
            ("https://t.example/7#section", "https://t.example/7#section"),
            ("not a url ?token=1", "not a url ?token=1"),
        ] {
            assert_eq!(strip_url(url, &|_| false), expected);
        }
        let value = json!({"title": "see https://a:b@x.example/?key=tok/en+42=&q=1", "n": 3, "list": ["tok\\/en+42="]});
        assert_eq!(
            clean_value(value, &secrets),
            json!({"title": "see https://***@x.example/?key=***&q=1", "n": 3, "list": ["***"]})
        );
    }

    #[test]
    fn urls_keep_public_parameters_and_lose_only_credentials() {
        let cleaner = Cleaner::new(["tok-3f2a1b".to_string()]);
        for (url, expected) in [
            // Kept: names that merely contain a credential word, and ordinary parameters.
            (
                "https://t.example/browse?issueKey=ENG-1&assignee=me&selectedIssue=ENG-1&filter=open&view=board",
                "https://t.example/browse?issueKey=ENG-1&assignee=me&selectedIssue=ENG-1&filter=open&view=board",
            ),
            ("https://t.example/i/ENG-1#comment=12&monkey=1", "https://t.example/i/ENG-1#comment=12&monkey=1"),
            // Dropped: exact credential names, decoded and in any case, and values holding a secret.
            ("https://u:p@t.example/i?access_token=a&view=1", "https://t.example/i?view=1"),
            (
                "https://t.example/i?API_KEY=a&apikey=b&client%5Fsecret=c&X-Amz-Signature=d&ref=7",
                "https://t.example/i?ref=7",
            ),
            ("https://t.example/i?state=tok%2D3f2a1b&page=2", "https://t.example/i?page=2"),
            ("https://t.example/i#id_token=x&tab=2", "https://t.example/i#tab=2"),
            ("https://t.example/i?Signature=s&X-Amz-Credential=c&x-amz-security-token=t", "https://t.example/i"),
        ] {
            assert_eq!(cleaner.url(url), expected, "{url}");
        }
    }

    #[test]
    fn the_cleaner_masks_split_secrets_in_place_and_withholds_what_it_cannot_place() {
        let cleaner = Cleaner::new(["sk-split-9e8d7c".to_string(), "tiny".to_string()]);
        assert_eq!(cleaner.secrets(), ["sk-split-9e8d7c"], "short secrets would mask words");
        for split in [
            "Rotate sk-split\n-9e8d7c now",
            "Rotate sk-split\\n-9e8d7c now",
            "Rotate sk-split\r\n  -9e8d7c now",
            "Rotate sk-split%0A-9e8d7c now",
            "Rotate sk%2Dsplit\\u002d9e8d7c now",
        ] {
            assert_eq!(cleaner.field(split), "Rotate *** now", "{split:?}");
        }
        // Encoded twice, the secret cannot be placed: the field goes.
        assert_eq!(cleaner.field("Rotate sk-split%252D9e8d7c now"), WITHHELD_FIELD);
        assert_eq!(cleaner.error("bad sk-split%252D9e8d7c"), WITHHELD);
        let value = json!({"sk-split-9e8d7c": "rejected", "nested": [{"sk-split\n-9e8d7c": 1, "ok": "fine"}],
            "link": "see https://t.example/b?issueKey=ENG-1&token=zz"});
        assert_eq!(
            cleaner.value(value),
            json!({"***": "rejected", "nested": [{"***": 1, "ok": "fine"}], "link": "see https://t.example/b?issueKey=ENG-1&token=***"})
        );
        assert_eq!(cleaner.with(["another-secret".to_string()]).field("another-secret"), "***");
        let long = format!("{} tail", "word ".repeat(MAX_FIELD_CHARS));
        assert_eq!(
            Cleaner::default().value(json!({"description": long})),
            json!({"description": long}),
            "agents get it whole"
        );
        let bounded = Cleaner::default().field(&long);
        assert!(bounded.ends_with("word…") && bounded.chars().count() <= MAX_FIELD_CHARS + 1, "cut between words");
    }

    #[test]
    fn redaction_hides_credentials_and_bounds_the_text() {
        let secrets = vec!["sk-live-123456".to_string(), "tiny".to_string()];
        for (input, expected) in [
            ("token sk-live-123456 echoed", "token *** echoed"),
            ("Authorization: Bearer abc.def==, more", "Authorization: Bearer ***, more"),
            ("{\"authorization\":\"Basic dXNlcjpwYXNz\"}", "{\"authorization\":\"Basic ***\"}"),
            ("authorization=s3cr3t-value next", "authorization=*** next"),
            (
                "GET https://me:pa55@host.example:8443/x?q=1&api_key=zz9&page=2#top",
                "GET https://***@host.example:8443/x?q=1&api_key=***&page=2#top",
            ),
            ("see https://host.example/a?Access_Token=abc", "see https://host.example/a?Access_Token=***"),
            ("the basic plan, a bearer of news", "the basic plan, a bearer of news"),
            ("unauthorized: no tiny secrets?", "unauthorized: no tiny secrets?"),
            ("Bearer", "Bearer"),
            ("café Bearer ëé-tok", "café Bearer ***"),
        ] {
            assert_eq!(redact(input, &secrets), expected, "{input}");
        }
        let long = redact(&"é".repeat(MAX_ERROR_CHARS * 2), &[]);
        assert_eq!(long.chars().count(), MAX_ERROR_CHARS + 1);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn readme_ticket_examples_are_whole_sections() {
        let readme = include_str!("../../../README.md");
        for block in readme.split("```toml\n").skip(1).filter_map(|rest| rest.split("```").next()) {
            if !block.lines().any(|line| line.trim() == "[tickets]") {
                continue;
            }
            let table: toml::Table = toml::from_str(block).unwrap();
            let tickets: TicketConfig = table["tickets"].clone().try_into().unwrap();
            // A workspace's section replaces the main config's, so a partial one switches tickets off.
            assert!(tickets.is_configured(), "incomplete [tickets] example in the README:\n{block}");
        }
    }

    #[test]
    fn inserted_values_are_never_substituted_again() {
        let config = config(
            "[operations.comment]\ntool = \"note\"\narguments = { text = \"{body} ({key})\", raw = \"{unknown}\" }\n",
        );
        let comment = config.operation("comment").unwrap();
        let ticket = TicketLink { key: Some("AL-{id}".into()), ..link("7") };
        let body = "Use {title} and {board} literally, and {body} too.";
        let vars = Vars::new("alpha").ticket(&ticket).with("body", Some(body));
        assert_eq!(
            comment.arguments(&vars).unwrap(),
            json!({"text": "Use {title} and {board} literally, and {body} too. (AL-{id})", "raw": "{unknown}"})
        );
        assert!(comment.arguments(&Vars::new("alpha").ticket(&ticket)).unwrap_err().contains("{body}"));
    }

    #[test]
    fn result_paths_default_to_the_field_names() {
        let config = config("[operations.get]\ntool = \"read\"\n");
        let read = config
            .operation("get")
            .unwrap()
            .ticket(&json!({"id":"1","key":"K-1","state":"Open","updated_at":"t"}), "s")
            .unwrap();
        assert_eq!(
            read,
            TicketLink {
                system: "s".into(),
                id: "1".into(),
                key: Some("K-1".into()),
                state: Some("Open".into()),
                updated_at: Some("t".into()),
                ..TicketLink::default()
            }
        );
    }

    #[test]
    fn token_variable_names_are_trimmed_once_when_read() {
        let server = |text: &str| toml::from_str::<TicketConfig>(text).unwrap().server;
        let http = server("server = { url = \"https://t.example/mcp\", token_env = \"  TRACKER_TOKEN \" }");
        assert_eq!(http.token_env.as_deref(), Some("TRACKER_TOKEN"));
        assert_eq!(http.bearer_env(), Some("TRACKER_TOKEN"));
        assert_eq!(server("server = { url = \"https://t.example/mcp\", token_env = \"  \" }").token_env, None);
        let stdio = server("server = { command = [\"tracker-mcp\"], token_env = \"TRACKER_TOKEN\" }");
        assert_eq!(stdio.bearer_env(), None, "IMACS gives a stdio server no token");
        assert_eq!(
            toml::from_str::<TicketConfig>(
                &toml::to_string(&TicketConfig { server: http.clone(), ..TicketConfig::default() }).unwrap()
            )
            .unwrap()
            .server,
            http
        );
    }

    #[test]
    fn config_parses_modes_policies_states_and_survives_toml() {
        let parsed = config(
            r#"
            system = "beta"
            mode = "ticket"
            comments = "chief"
            workflow = "One ticket per plan."
            timeout_secs = 5
            [server]
            url = "https://beta.example/mcp"
            token_env = "BETA_TOKEN"
            [states]
            running = "In Progress"
            done = " Done "
            "#,
        );
        assert_eq!(parsed.mode, TicketMode::Ticket);
        assert_eq!(parsed.comments, CommentPolicy::Chief);
        assert!(parsed.is_configured());
        assert_eq!(parsed.states.get(TaskStatus::Done), Some("Done"));
        assert_eq!(parsed.states.get(TaskStatus::Review), None);
        assert_eq!(parsed.states.status_of("in progress"), Some(TaskStatus::Running));
        assert_eq!(parsed.states.status_of("Backlog"), None);
        let saved = toml::to_string(&parsed).unwrap();
        assert_eq!(toml::from_str::<TicketConfig>(&saved).unwrap(), parsed);
        assert_eq!(TicketConfig::default().timeout_secs, 30);
        assert!(toml::from_str::<TicketConfig>("mode = \"kanban\"").is_err());
        assert!(toml::from_str::<TicketConfig>("comments = \"everyone\"").is_err());
    }

    #[test]
    fn comment_policy_decides_who_posts() {
        for (policy, agent, chief) in [
            (CommentPolicy::Agents, true, true),
            (CommentPolicy::Chief, false, true),
            (CommentPolicy::Off, false, false),
        ] {
            let config = TicketConfig { comments: policy, ..TicketConfig::default() };
            assert_eq!(config.posts(&Author::Agent("GPT".into())), agent);
            for author in [Author::Chief("Claude".into()), Author::User, Author::Imacs] {
                assert_eq!(config.posts(&author), chief);
            }
        }
    }

    #[test]
    fn plan_status_follows_its_tasks() {
        let plan = |statuses: &[TaskStatus]| Plan {
            id: "p".into(),
            tasks: statuses.iter().map(|s| Task { status: *s, ..Task::default() }).collect(),
            ..Plan::default()
        };
        use TaskStatus::*;
        for (statuses, expected) in [
            (vec![], Planned),
            (vec![Planned, Planned], Planned),
            (vec![Running, Planned], Running),
            (vec![Done, Planned], Running),
            (vec![Review, Done], Review),
            (vec![Review, Planned], Running),
            (vec![Blocked, Running], Blocked),
            (vec![Done, Done], Done),
        ] {
            assert_eq!(plan_status(&plan(&statuses)), expected, "{statuses:?}");
        }
        let completed = Plan { status: PlanStatus::Completed, ..plan(&[Review]) };
        assert_eq!(plan_status(&completed), Done);
    }

    #[test]
    fn signatures_name_the_model_and_effort() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            model_label("codex", &args(&["-m", "gpt-6.1-sol", "-c", "model_reasoning_effort=\"high\""])),
            "GPT-6.1 Sol (high)"
        );
        assert_eq!(model_label("claude", &args(&["--model", "claude-opus-5-5"])), "Claude Opus 5.5");
        assert_eq!(
            model_label("claude", &args(&["--model=claude-sonnet-4-5-20250929", "--effort", "max"])),
            "Claude Sonnet 4.5 (max)"
        );
        assert_eq!(model_label("codex", &args(&["-c", "model=gpt-5-codex"])), "GPT-5 Codex");
        assert_eq!(model_label("codex", &[]), "Codex");
        assert_eq!(model_label("", &[]), "Claude");
        assert_eq!(pretty_model("gpt-6.1-terra"), "GPT-6.1 Terra");
    }

    #[test]
    fn comments_are_signed_and_carry_the_work() {
        let task = Task {
            id: "t2".into(),
            title: "Add the --json flag".into(),
            brief: "Use serde.".into(),
            acceptance: vec!["tests pass".into()],
            branch: Some("imacs/t2".into()),
            ..Task::default()
        };
        let chief = Author::Chief("Claude Opus 5.5".into());
        let agent = Author::Agent("GPT-6.1 Sol (high)".into());
        assert_eq!(
            dispatch_comment(&chief, &task, "GPT-6.1 Sol (high)"),
            "This is Claude Opus 5.5, the IMACS chief.\nDispatched task t2 (Add the --json flag) to GPT-6.1 Sol (high) \
             on branch imacs/t2.\n\nImplementation brief:\nUse serde.\n\nDone when:\n- tests pass"
        );
        assert_eq!(
            report_comment(&agent, &task, "done", " Added it. "),
            "This is GPT-6.1 Sol (high), the IMACS agent on task t2 (Add the --json flag).\nReported done:\n\nAdded it."
        );
        assert!(status_comment(&chief, &task, TaskStatus::Done, Some("verified")).ends_with("accepted.\n\nverified"));
        assert!(
            status_comment(&Author::Imacs, &task, TaskStatus::Blocked, None).starts_with("Posted by IMACS.\nTask t2")
        );
        assert!(
            status_comment(&Author::User, &task, TaskStatus::Done, None).starts_with("Posted by IMACS for the user.")
        );
        let plan = Plan {
            id: "p".into(),
            goal: "Ship".into(),
            tasks: vec![Task { status: TaskStatus::Done, ..task }],
            ..Plan::default()
        };
        assert_eq!(
            completed_comment(&chief, &plan),
            "This is Claude Opus 5.5, the IMACS chief.\nPlan complete: Ship (1 of 1 tasks done, integrated on imacs/plan-p)."
        );
        assert_eq!(
            chief_comment(&chief, " Uses the v2 API. "),
            "This is Claude Opus 5.5, the IMACS chief.\n\nUses the v2 API."
        );
    }

    #[test]
    fn prompt_section_describes_the_configured_workflow() {
        let board = TicketConfig {
            system: "alpha".into(),
            workflow: "Tickets are written by the product owner.".into(),
            states: StateMap { running: Some("Doing".into()), done: Some("Done".into()), ..StateMap::default() },
            ..TicketConfig::default()
        };
        let text = board.prompt_section();
        assert!(text.starts_with("Tickets: this workspace tracks its work in alpha"));
        assert!(
            text.contains("Mode board") && text.contains("tasks[].ticket.id") && text.contains("imacs_ticket_sync")
        );
        assert!(text.contains("running → Doing, done → Done"));
        assert!(text.contains("signed with their model") && text.contains("include ticket in statuses"));
        assert!(text.ends_with("Workflow: Tickets are written by the product owner."));
        let ticket = TicketConfig {
            system: "beta".into(),
            mode: TicketMode::Ticket,
            comments: CommentPolicy::Off,
            ..TicketConfig::default()
        };
        let text = ticket.prompt_section();
        assert!(text.contains("one ticket maps to one plan") && text.contains("do not comment"));
        assert!(!text.contains("Workflow:") && !text.contains("IMACS sets ticket states"));
    }
}
