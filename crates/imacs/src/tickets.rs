//! The ticket worker: runs the generic ticket operations against the configured MCP
//! server on a thread of its own, so a slow or broken ticket system never holds up
//! IMACS. Jobs run one at a time, in order, over one kept-open connection.

use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use imacs_core::tickets::{self, McpServer, TicketConfig, TicketLink, Vars};
use serde_json::Value;

use crate::mcp_client::{McpClient, McpError};

/// After a connection failure, jobs fail at once for this long instead of each
/// waiting for a server that is down.
const BACKOFF: Duration = Duration::from_secs(15);

/// How long a variable that was set nowhere is not looked up again (each lookup may start
/// a login shell).
const MISSING_ENV: Duration = Duration::from_secs(60);

pub enum TicketOp {
    Get(TicketLink),
    /// Read full ticket details for a scoped agent request, without refreshing other links.
    Read(TicketLink),
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
    Read(Value),
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
                worker.prepare(&job.config);
                let result = worker.run(&job.config, job.op);
                let result = worker.clean(result);
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
    /// The connection, with the server and token it was made with.
    client: Option<(McpServer, Option<String>, McpClient)>,
    /// The token the current job's connection should authenticate with, as it resolves now.
    token: Option<String>,
    /// The server that failed, when, and its cleaned error.
    down: Option<(McpServer, Instant, String)>,
    /// Cleans what the current job returns, with that job's secrets.
    cleaner: tickets::Cleaner,
    /// Variables recently found set nowhere.
    missing: std::collections::HashMap<String, Instant>,
}

impl Worker {
    /// Resolves the job's credentials as they are now. Everything the job brings back is
    /// cleaned with them, so a result arriving after a config change never carries the old
    /// ones. A connection made with another token is replaced, and the job is cleaned for
    /// that token too until the old connection is gone.
    fn prepare(&mut self, config: &TicketConfig) {
        self.token = config.server.bearer_env().map(str::to_string).and_then(|name| self.env(&name));
        let secrets = tickets::secrets_of(config, |name| self.env(name));
        let mut cleaner = tickets::Cleaner::new(secrets);
        if let Some((_, used, _)) = &self.client
            && *used != self.token
        {
            cleaner = cleaner.with(used.clone());
            self.client = None;
        }
        self.cleaner = cleaner;
    }

    /// A variable as IMACS or the login shell sets it now. One set nowhere is not looked up
    /// again for a while: each lookup may start a login shell.
    fn env(&mut self, name: &str) -> Option<String> {
        if self.missing.get(name).is_some_and(|at| at.elapsed() < MISSING_ENV) {
            return std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        }
        let value = crate::setup::env_var(name);
        match &value {
            None => self.missing.insert(name.to_string(), Instant::now()),
            Some(_) => self.missing.remove(name),
        };
        value
    }

    /// Errors and everything read from the server, before it leaves the worker.
    fn clean(&self, result: Result<TicketOutput, String>) -> Result<TicketOutput, String> {
        let error = |error: String| self.cleaner.error(&error);
        let link = |link: TicketLink| self.cleaner.link(link);
        Ok(match result.map_err(error)? {
            TicketOutput::Ticket(ticket) => TicketOutput::Ticket(link(ticket)),
            TicketOutput::Tickets(tickets) => TicketOutput::Tickets(tickets.into_iter().map(link).collect()),
            TicketOutput::Read(value) => TicketOutput::Read(self.cleaner.value(value)),
            TicketOutput::Pushed(read) => TicketOutput::Pushed(read.map(|read| read.map(link).map_err(error))),
        })
    }

    fn run(&mut self, config: &TicketConfig, op: TicketOp) -> Result<TicketOutput, String> {
        let system = config.system.as_str();
        match op {
            TicketOp::Read(link) => self.call(config, "get", &Vars::new(system).ticket(&link)).map(TicketOutput::Read),
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
        if let Some((server, since, error)) = &self.down
            && *server == config.server
            && since.elapsed() < BACKOFF
        {
            return Err(format!("{} is unavailable: {error}", config.system));
        }
        if self.client.as_ref().is_none_or(|(server, used, _)| *server != config.server || *used != self.token) {
            self.client = None;
            let timeout = Duration::from_secs(config.timeout_secs.max(1));
            // The connection authenticates with the token this job resolved, and the worker records
            // the token the connection reports using, so the two can never quietly differ.
            let token = self.token.clone();
            let bearer = config.server.bearer_env().map(str::to_string);
            let env = |name: &str| {
                if bearer.as_deref() == Some(name) { token.clone() } else { crate::setup::env_var(name) }
            };
            match McpClient::connect(&config.server, timeout, env) {
                Ok(client) => {
                    let used = client.token().map(str::to_string);
                    self.client = Some((config.server.clone(), used, client));
                }
                Err(err) => return Err(self.broken(&config.server, err)),
            }
        }
        let (_, _, client) = self.client.as_mut().expect("connected");
        match client.call_tool(&operation.tool, arguments) {
            Ok(result) => {
                self.down = None;
                Ok(imacs_core::tickets::tool_output(&result))
            }
            Err(err) => Err(self.broken(&config.server, err)),
        }
    }

    /// The backoff remembers the failure cleaned with this job's secrets: a later job may
    /// run with others.
    fn broken(&mut self, server: &McpServer, err: McpError) -> String {
        if let McpError::Transport(message) = &err {
            self.client = None;
            self.down = Some((server.clone(), Instant::now(), self.cleaner.error(message)));
        }
        err.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc::Receiver;

    /// A streamable HTTP MCP server whose every tool call fails, echoing the bearer token
    /// it was called with. Each request's Authorization header goes to the receiver.
    fn echo_server() -> (String, Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let (tx, tokens) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let (mut length, mut token) = (0, String::new());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(value) = lower.strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                    if lower.starts_with("authorization: bearer ") {
                        token = line["authorization: bearer ".len()..].trim().to_string();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let answer = match body["method"].as_str().unwrap_or_default() {
                    "initialize" => Some(json!({"protocolVersion": "2025-06-18", "capabilities": {}})),
                    "tools/call" => {
                        let _ = tx.send(token.clone());
                        let text = format!("credential {token} rejected");
                        Some(json!({"isError": true, "content": [{"type": "text", "text": text}]}))
                    }
                    _ => None,
                };
                let response = match answer {
                    Some(result) => {
                        let text = json!({"jsonrpc": "2.0", "id": body["id"], "result": result}).to_string();
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                            text.len()
                        )
                    }
                    None => "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
                };
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (url, tokens)
    }

    #[test]
    fn a_rotated_token_reconnects_and_is_cleaned_together_with_the_old_one() {
        const CHILD: &str = "IMACS_TICKET_WORKER_TOKEN_CHILD";
        const NAME: &str = "tickets::tests::a_rotated_token_reconnects_and_is_cleaned_together_with_the_old_one";
        if std::env::var_os(CHILD).is_none() {
            // The test changes an environment variable, so it runs alone in a process of its own.
            let output = crate::test_support::output(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", NAME, "--nocapture"])
                    .env(CHILD, "1"),
            );
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // The token variable as written in a config, padded or not: the name is the same.
        for (round, name) in ["ECHO_TEST_TOKEN", " ECHO_TEST_TOKEN "].into_iter().enumerate() {
            let old = format!("oldkey-7c1d9e{round}");
            let new = format!("newkey-2b3c4d{round}");
            let (url, tokens) = echo_server();
            let config: TicketConfig = toml::from_str(&format!(
                "system = \"echo\"\nserver = {{ url = \"{url}\", token_env = \"{name}\" }}\n\
                 [operations.comment]\ntool = \"note\"\narguments = {{ body = \"{{body}}\" }}"
            ))
            .unwrap();
            let config = Arc::new(config);
            let (done, results) = crate::host::Wake::default().channel();
            let jobs = spawn(done);
            let comment = |n: u32| {
                let op = TicketOp::Comment(TicketLink { id: "1".into(), ..TicketLink::default() }, "Hi".into());
                jobs.send(TicketJob { config: config.clone(), op, tag: n }).unwrap();
                let (tag, result) = results.recv_timeout(Duration::from_secs(10)).unwrap();
                assert_eq!(tag, n);
                result.err().unwrap()
            };
            // SAFETY: this process runs only this test, and the worker reads the variable only
            // while a job runs; each job is awaited before the variable changes.
            unsafe { std::env::set_var("ECHO_TEST_TOKEN", &old) };
            assert_eq!(comment(1), "note failed: credential *** rejected", "{name:?}");
            assert_eq!(tokens.recv().unwrap(), old);
            unsafe { std::env::set_var("ECHO_TEST_TOKEN", &new) };
            let rotated = comment(2);
            assert_eq!(tokens.recv().unwrap(), new, "{name:?}: the connection authenticates with the new token");
            assert!(!rotated.contains(&old) && !rotated.contains(&new), "{name:?}: {rotated}");
        }
    }
}
