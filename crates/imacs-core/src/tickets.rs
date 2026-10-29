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
    /// Environment variable holding a bearer token for `url`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
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
        }
    }
}

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
    /// Configured with a system name and a server to reach it.
    pub fn is_configured(&self) -> bool {
        !self.system.trim().is_empty()
            && (!self.server.command.is_empty() || self.server.url.as_deref().is_some_and(|u| !u.trim().is_empty()))
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
            state: field(&self.state),
            updated_at: field(&self.updated_at),
            ..TicketLink::default()
        }
    }
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

    fn link(id: &str) -> TicketLink {
        TicketLink { system: "alpha".into(), id: id.into(), ..TicketLink::default() }
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
