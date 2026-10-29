//! The ticket worker: runs the generic ticket operations against the configured MCP
//! server on a thread of its own, so a slow or broken ticket system never holds up
//! IMACS. Jobs run one at a time, in order, over one kept-open connection.

use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use imacs_core::tickets::{McpServer, TicketConfig, TicketLink, Vars};
use serde_json::Value;

use crate::mcp_client::{McpClient, McpError};

/// After a connection failure, jobs fail at once for this long instead of each
/// waiting for a server that is down.
const BACKOFF: Duration = Duration::from_secs(15);

pub enum TicketOp {
    Get(TicketLink),
    /// The tickets on a board.
    List(TicketLink),
    UpdateState(TicketLink, String),
    Comment(TicketLink, String),
    Create {
        board: Option<TicketLink>,
        title: String,
        body: String,
    },
}

pub enum TicketOutput {
    Ticket(TicketLink),
    Tickets(Vec<TicketLink>),
    /// A push went through; the ticket as read right after, when `get` is configured.
    Pushed(Option<Result<TicketLink, String>>),
}

pub struct TicketJob<T> {
    pub config: Arc<TicketConfig>,
    pub op: TicketOp,
    pub tag: T,
}

/// Starts the worker. Each finished job comes back on `done` with its tag.
pub fn spawn<T: Send + 'static>(done: crate::host::Sender<(T, Result<TicketOutput, String>)>) -> Sender<TicketJob<T>> {
    let (tx, rx) = std::sync::mpsc::channel::<TicketJob<T>>();
    std::thread::Builder::new()
        .name("imacs-tickets".into())
        .spawn(move || {
            let mut worker = Worker::default();
            while let Ok(job) = rx.recv() {
                let result = worker.run(&job.config, job.op);
                if done.send((job.tag, result)).is_err() {
                    break;
                }
            }
        })
        .expect("ticket worker thread");
    tx
}

#[derive(Default)]
struct Worker {
    client: Option<(McpServer, McpClient)>,
    down: Option<(Instant, String)>,
}

impl Worker {
    fn run(&mut self, config: &TicketConfig, op: TicketOp) -> Result<TicketOutput, String> {
        let system = config.system.as_str();
        match op {
            TicketOp::Get(link) => {
                let output = self.call(config, "get", &Vars::new(system).ticket(&link))?;
                Ok(TicketOutput::Ticket(config.operation("get").expect("called").ticket(&output, system)?))
            }
            TicketOp::List(board) => {
                let output = self.call(config, "list", &Vars::new(system).board(Some(&board)))?;
                Ok(TicketOutput::Tickets(config.operation("list").expect("called").tickets(&output, system)?))
            }
            TicketOp::UpdateState(link, state) => {
                self.call(config, "update_state", &Vars::new(system).ticket(&link).with("state", Some(&state)))?;
                Ok(TicketOutput::Pushed(self.reread(config, link)))
            }
            TicketOp::Comment(link, body) => {
                self.call(config, "comment", &Vars::new(system).ticket(&link).with("body", Some(&body)))?;
                Ok(TicketOutput::Pushed(self.reread(config, link)))
            }
            TicketOp::Create { board, title, body } => {
                let vars =
                    Vars::new(system).board(board.as_ref()).with("title", Some(&title)).with("body", Some(&body));
                let output = self.call(config, "create", &vars)?;
                let created = config.operation("create").expect("called").ticket(&output, system)?;
                if created.id.is_empty() {
                    return Err("the create operation's result has no id: set operations.create.result.id".into());
                }
                Ok(TicketOutput::Ticket(created))
            }
        }
    }

    /// The ticket right after a push, so the card shows what the system now says.
    fn reread(&mut self, config: &TicketConfig, link: TicketLink) -> Option<Result<TicketLink, String>> {
        config.operation("get")?;
        Some(match self.run(config, TicketOp::Get(link)) {
            Ok(TicketOutput::Ticket(ticket)) => Ok(ticket),
            Ok(_) => Err("unexpected get result".into()),
            Err(err) => Err(err),
        })
    }

    /// Calls the tool behind a generic operation and returns what its result paths read from.
    fn call(&mut self, config: &TicketConfig, name: &str, vars: &Vars) -> Result<Value, String> {
        let operation = config.operation(name).ok_or_else(|| format!("operations.{name} is not configured"))?;
        let arguments = operation.arguments(vars)?;
        if let Some((since, error)) = &self.down
            && since.elapsed() < BACKOFF
        {
            return Err(format!("{} is unavailable: {error}", config.system));
        }
        if self.client.as_ref().is_none_or(|(server, _)| *server != config.server) {
            self.client = None;
            let timeout = Duration::from_secs(config.timeout_secs.max(1));
            match McpClient::connect(&config.server, timeout, crate::setup::env_var) {
                Ok(client) => self.client = Some((config.server.clone(), client)),
                Err(err) => return Err(self.broken(err)),
            }
        }
        let (_, client) = self.client.as_mut().expect("connected");
        match client.call_tool(&operation.tool, arguments) {
            Ok(result) => {
                self.down = None;
                Ok(imacs_core::tickets::tool_output(&result))
            }
            Err(err) => Err(self.broken(err)),
        }
    }

    fn broken(&mut self, err: McpError) -> String {
        if let McpError::Transport(message) = &err {
            self.client = None;
            self.down = Some((Instant::now(), message.clone()));
        }
        err.to_string()
    }
}
