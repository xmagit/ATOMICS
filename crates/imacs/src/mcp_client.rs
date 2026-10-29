//! A small MCP client: IMACS calling tools on another MCP server, over stdio or
//! streamable HTTP. The ticket layer uses it; nothing here knows about tickets.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use imacs_core::tickets::McpServer;
use serde_json::{Value, json};

const PROTOCOL_VERSION: &str = "2025-06-18";
const STDERR_TAIL: usize = 2000;

/// Why a call failed: the connection, which is worth reconnecting and backing off
/// from, or the server answering with an error over a connection that is fine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpError {
    Transport(String),
    Server(String),
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpError::Transport(message) | McpError::Server(message) => f.write_str(message),
        }
    }
}

pub struct McpClient {
    transport: Transport,
    next_id: u64,
    timeout: Duration,
}

enum Transport {
    Stdio(StdioServer),
    Http(HttpServer),
}

struct StdioServer {
    program: String,
    child: Child,
    stdin: Sender<String>,
    writes: Receiver<std::io::Result<()>>,
    lines: Receiver<String>,
    stderr: Arc<Mutex<String>>,
}

struct HttpServer {
    agent: ureq::Agent,
    url: String,
    token: Option<String>,
    session: Option<String>,
    initialized: bool,
}

impl McpClient {
    /// Starts or reaches the server and completes MCP's initialize handshake. `env`
    /// looks up the HTTP token variable.
    pub fn connect(
        server: &McpServer,
        timeout: Duration,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<McpClient, McpError> {
        let transport = match (server.command.split_first(), server.url.as_deref().map(str::trim)) {
            (Some((program, args)), _) => Transport::Stdio(StdioServer::spawn(program, args, server)?),
            (None, Some(url)) if !url.is_empty() => {
                let token = match server.token_env.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                    Some(name) => Some(env(name).ok_or_else(|| McpError::Transport(format!("{name} is not set")))?),
                    None => None,
                };
                let agent = ureq::Agent::config_builder()
                    .timeout_global(Some(timeout))
                    .http_status_as_error(false)
                    .build()
                    .into();
                Transport::Http(HttpServer { agent, url: url.to_string(), token, session: None, initialized: false })
            }
            _ => return Err(McpError::Transport("no MCP server configured: set server.command or server.url".into())),
        };
        let mut client = McpClient { transport, next_id: 0, timeout };
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "imacs", "title": "IMACS", "version": env!("CARGO_PKG_VERSION") },
        });
        client.request("initialize", params).map_err(|err| match err {
            McpError::Server(message) => McpError::Transport(format!("initialize failed: {message}")),
            transport => transport,
        })?;
        if let Transport::Http(http) = &mut client.transport {
            http.initialized = true;
        }
        client.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }), None)?;
        Ok(client)
    }

    /// Calls a tool and returns its result. A result flagged `isError` is an error.
    pub fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value, McpError> {
        let result = self.request("tools/call", json!({ "name": name, "arguments": arguments }))?;
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            let text = result
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            return Err(McpError::Server(format!(
                "{name} failed: {}",
                if text.trim().is_empty() { "no details" } else { text.trim() }
            )));
        }
        Ok(result)
    }

    /// Lists all offered tools, following MCP pagination without calling any tool.
    pub fn tool_names(&mut self) -> Result<Vec<String>, McpError> {
        let mut names = Vec::new();
        let mut cursors = std::collections::HashSet::new();
        let mut params = json!({});
        loop {
            let page = self.request("tools/list", params)?;
            let tools =
                page["tools"].as_array().ok_or_else(|| McpError::Server("tools/list has no tools array".into()))?;
            for tool in tools {
                let name = tool["name"]
                    .as_str()
                    .ok_or_else(|| McpError::Server("tools/list has a tool without a name".into()))?;
                names.push(name.to_string());
            }
            let Some(cursor) = page["nextCursor"].as_str().filter(|s| !s.is_empty()) else { break };
            if !cursors.insert(cursor.to_string()) || cursors.len() > 100 {
                return Err(McpError::Server("tools/list pagination did not finish".into()));
            }
            params = json!({"cursor": cursor});
        }
        Ok(names)
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, McpError> {
        self.next_id += 1;
        let id = self.next_id;
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let response = self.send(&message, Some(id))?.unwrap_or(Value::Null);
        if let Some(error) = response.get("error") {
            let text = error.get("message").and_then(Value::as_str).map(str::to_string).unwrap_or(error.to_string());
            return Err(McpError::Server(format!("{method}: {text}")));
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    fn send(&mut self, message: &Value, id: Option<u64>) -> Result<Option<Value>, McpError> {
        match &mut self.transport {
            Transport::Stdio(server) => server.exchange(message, id, self.timeout),
            Transport::Http(server) => server.exchange(message, id, Instant::now() + self.timeout),
        }
    }
}

impl StdioServer {
    fn spawn(program: &str, args: &[String], server: &McpServer) -> Result<StdioServer, McpError> {
        let mut command = Command::new(crate::setup::resolve_program(program));
        command.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // Launched from the Dock, IMACS's own PATH lacks what the login shell adds (npx, brew).
        if let Some(path) = crate::setup::SHELL_PATH.as_ref() {
            command.env("PATH", path);
        }
        command.envs(&server.env);
        let mut child =
            command.spawn().map_err(|err| McpError::Transport(format!("starting {program} failed: {err}")))?;
        let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let (Some(mut stdin), Some(stdout), Some(mut stderr_pipe)) = (stdin, stdout, stderr) else {
            let _ = child.kill();
            return Err(McpError::Transport(format!("{program} has no stdio")));
        };
        // A full pipe must not block the ticket worker. Wait for each write using
        // the same deadline as its response, including replies to server requests.
        let (input, messages) = std::sync::mpsc::channel::<String>();
        let (written, writes) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(message) = messages.recv() {
                let result = writeln!(stdin, "{message}").and_then(|_| stdin.flush());
                let failed = result.is_err();
                if written.send(result).is_err() || failed {
                    break;
                }
            }
        });
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let tail = stderr.clone();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 1024];
            while let Ok(read) = stderr_pipe.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                let mut tail = tail.lock().unwrap();
                tail.push_str(&String::from_utf8_lossy(&buffer[..read]));
                if tail.len() > STDERR_TAIL * 2 {
                    let mut cut = tail.len() - STDERR_TAIL;
                    while !tail.is_char_boundary(cut) {
                        cut += 1;
                    }
                    tail.drain(..cut);
                }
            }
        });
        Ok(StdioServer { program: program.to_string(), child, stdin: input, writes, lines, stderr })
    }

    fn exchange(&mut self, message: &Value, id: Option<u64>, timeout: Duration) -> Result<Option<Value>, McpError> {
        let deadline = Instant::now() + timeout;
        self.write(message, deadline, timeout)?;
        let Some(id) = id else { return Ok(None) };
        loop {
            if Instant::now() >= deadline {
                return Err(self.timed_out(timeout));
            }
            let line = match self.lines.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => return Err(self.timed_out(timeout)),
                Err(RecvTimeoutError::Disconnected) => return Err(self.gone("it exited")),
            };
            // Servers that print banners or logs to stdout still work.
            let Ok(value) = serde_json::from_str::<Value>(&line) else { continue };
            if value.get("method").is_some() {
                if let Some(reply) = server_reply(&value) {
                    self.write(&reply, deadline, timeout)?;
                }
                continue;
            }
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                return Ok(Some(value));
            }
        }
    }

    fn write(&mut self, message: &Value, deadline: Instant, timeout: Duration) -> Result<(), McpError> {
        if self.stdin.send(message.to_string()).is_err() {
            return Err(self.gone("writing to it failed"));
        }
        match self.writes.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(self.gone(&format!("writing to it failed: {err}"))),
            Err(RecvTimeoutError::Timeout) => Err(self.timed_out(timeout)),
            Err(RecvTimeoutError::Disconnected) => Err(self.gone("writing to it failed")),
        }
    }

    fn timed_out(&mut self, timeout: Duration) -> McpError {
        let _ = self.child.kill();
        McpError::Transport(format!("{} did not answer within {} s", self.program, timeout.as_secs_f32()))
    }

    /// The server stopped answering: say how it ended, with the end of its stderr.
    fn gone(&mut self, what: &str) -> McpError {
        let deadline = Instant::now() + Duration::from_millis(200);
        let mut status = self.child.try_wait().ok().flatten();
        while status.is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            status = self.child.try_wait().ok().flatten();
        }
        let mut message = format!("{} {what}", self.program);
        if let Some(status) = status {
            message.push_str(&format!(" ({status})"));
        }
        let stderr = self.stderr.lock().unwrap();
        let tail = stderr.trim();
        if !tail.is_empty() {
            let start = tail.char_indices().rev().nth(STDERR_TAIL.min(400)).map(|(i, _)| i).unwrap_or(0);
            message.push_str(&format!(": {}", &tail[start..]));
        }
        McpError::Transport(message)
    }
}

impl Drop for StdioServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl HttpServer {
    fn exchange(&mut self, message: &Value, id: Option<u64>, deadline: Instant) -> Result<Option<Value>, McpError> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(McpError::Transport(format!("request to {} timed out", self.url)));
        }
        let mut request = self
            .agent
            .post(&self.url)
            .config()
            .timeout_global(Some(remaining))
            .build()
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        if let Some(token) = &self.token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        if let Some(session) = &self.session {
            request = request.header("Mcp-Session-Id", session);
        }
        if self.initialized {
            request = request.header("MCP-Protocol-Version", PROTOCOL_VERSION);
        }
        let mut response = request
            .send(message.to_string())
            .map_err(|err| McpError::Transport(format!("request to {} failed: {err}", self.url)))?;
        if let Some(session) = response.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            self.session = Some(session.to_string());
        }
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !(200..300).contains(&status) {
            let body = response
                .body_mut()
                .read_to_string()
                .map_err(|err| McpError::Transport(format!("reading the answer from {} failed: {err}", self.url)))?;
            let hint = if matches!(status, 401 | 403) { " (check server.token_env)" } else { "" };
            let body: String = body.chars().take(300).collect();
            return Err(McpError::Transport(format!("{} answered HTTP {status}{hint}: {}", self.url, body.trim())));
        }
        let Some(id) = id else { return Ok(None) };
        if content_type.starts_with("text/event-stream") {
            let mut reader = BufReader::new(response.body_mut().as_reader());
            loop {
                if Instant::now() >= deadline {
                    return Err(McpError::Transport(format!("request to {} timed out", self.url)));
                }
                let data = read_sse_event(&mut reader).map_err(|err| {
                    McpError::Transport(format!("reading the answer from {} failed: {err}", self.url))
                })?;
                let Some(data) = data else { break };
                if let Ok(value) = serde_json::from_str(&data)
                    && let Some(found) = self.handle_message(value, id, deadline)?
                {
                    return Ok(Some(found));
                }
            }
        } else {
            let body = response
                .body_mut()
                .read_to_string()
                .map_err(|err| McpError::Transport(format!("reading the answer from {} failed: {err}", self.url)))?;
            if let Ok(value) = serde_json::from_str(&body)
                && let Some(found) = self.handle_message(value, id, deadline)?
            {
                return Ok(Some(found));
            }
        }
        Err(McpError::Transport(format!("{} sent no answer to request {id}", self.url)))
    }

    fn handle_message(&mut self, value: Value, id: u64, deadline: Instant) -> Result<Option<Value>, McpError> {
        let messages = match value {
            Value::Array(items) => items,
            value => vec![value],
        };
        for value in messages {
            if value.get("method").is_some() {
                if let Some(reply) = server_reply(&value) {
                    // MCP responses are separate POSTs, even while the original
                    // response stream stays open. They share its time budget.
                    self.exchange(&reply, None, deadline)?;
                }
            } else if value.get("id").and_then(Value::as_u64) == Some(id) {
                return Ok(Some(value));
            }
        }
        Ok(None)
    }
}

fn server_reply(value: &Value) -> Option<Value> {
    let id = value.get("id")?;
    Some(if value["method"] == "ping" {
        json!({ "jsonrpc": "2.0", "id": id, "result": {} })
    } else {
        json!({ "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": "IMACS does not support this request" } })
    })
}

/// Read only as far as the next event, so an open stream can deliver an answer.
fn read_sse_event(reader: &mut impl BufRead) -> std::io::Result<Option<String>> {
    let mut data = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok((!data.is_empty()).then(|| data.join("\n")));
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if !data.is_empty() {
                return Ok(Some(data.join("\n")));
            }
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value).to_string());
        } else if line == "data" {
            data.push(String::new());
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;

    /// Exercise the shipped commented blocks rather than maintaining test-only profiles.
    pub(crate) fn example_profile(name: &str, state: &std::path::Path) -> imacs_core::tickets::TicketConfig {
        let template = imacs_core::config::DEFAULT_CONFIG;
        let block = template
            .split(&format!("# BEGIN {name} PROFILE\n"))
            .nth(1)
            .unwrap()
            .split(&format!("# END {name} PROFILE"))
            .next()
            .unwrap();
        let text =
            block.lines().skip(1).map(|line| line.strip_prefix("# ").unwrap_or("")).collect::<Vec<_>>().join("\n");
        let config: imacs_core::config::Config = toml::from_str(&text).unwrap();
        let mut tickets = config.tickets;
        tickets.server.command = vec![fake_server().display().to_string(), state.display().to_string()];
        tickets
    }

    /// The fake ticket server built from `examples/fake_ticket_server.rs`.
    pub(crate) fn fake_server() -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let examples = exe.parent().and_then(|deps| deps.parent()).unwrap().join("examples");
        let path = examples.join("fake_ticket_server");
        if path.exists() {
            return path;
        }
        // Cargo keeps the hashed binary next to the uplifted name.
        std::fs::read_dir(&examples)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .find(|p| {
                p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("fake_ticket_server-"))
                    && p.extension().is_none()
            })
            .unwrap_or_else(|| {
                panic!("{} is missing: run cargo build -p imacs --example fake_ticket_server first", path.display())
            })
    }

    /// A state file for the fake server holding these tickets.
    pub(crate) fn fake_state(dir: &std::path::Path, tickets: Value) -> PathBuf {
        let path = dir.join("tickets.json");
        std::fs::write(&path, json!({ "tickets": tickets, "calls": [], "fail": [], "hang": [] }).to_string()).unwrap();
        path
    }

    fn stdio(command: Vec<String>) -> McpServer {
        McpServer { command, ..McpServer::default() }
    }

    #[test]
    fn stdio_calls_tools_and_separates_server_errors_from_broken_connections() {
        let dir = tempfile::tempdir().unwrap();
        let state = fake_state(
            dir.path(),
            json!([{ "id": "7", "key": "AL-7", "board": "b1", "title": "Ship", "state": "Todo", "url": "https://t/7" }]),
        );
        let server = stdio(vec![fake_server().display().to_string(), state.display().to_string()]);
        let mut client = McpClient::connect(&server, Duration::from_secs(10), |_| None).unwrap();
        let result = client.call_tool("card_get", json!({ "card": "AL-7" })).unwrap();
        assert_eq!(result["structuredContent"]["card"]["column"]["title"], "Todo");
        client.call_tool("card_move", json!({ "card": "7", "column": "Doing" })).unwrap();
        let read = client.call_tool("issue_read", json!({ "id": "AL-7" })).unwrap();
        assert!(read["content"][0]["text"].as_str().unwrap().contains("\"status\":\"Doing\""));
        let unknown = client.call_tool("nope", json!({})).unwrap_err();
        assert!(matches!(&unknown, McpError::Server(m) if m.contains("unknown tool nope")), "{unknown:?}");
        let mut saved: Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
        saved["fail"] = json!(["card_note"]);
        std::fs::write(&state, saved.to_string()).unwrap();
        let failed = client.call_tool("card_note", json!({ "card": "7", "text": "hi" })).unwrap_err();
        assert_eq!(failed, McpError::Server("card_note failed: card_note is broken today".into()));
        client.call_tool("card_get", json!({ "card": "7" })).expect("a tool error leaves the connection usable");

        let mut slow = McpClient::connect(&server, Duration::from_millis(500), |_| None).unwrap();
        saved["hang"] = json!(["card_get"]);
        std::fs::write(&state, saved.to_string()).unwrap();
        let started = Instant::now();
        let hung = slow.call_tool("card_get", json!({ "card": "7" })).unwrap_err();
        assert!(matches!(&hung, McpError::Transport(m) if m.contains("did not answer")), "{hung:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn stdio_tolerates_banners_and_reports_how_a_server_ended() {
        let dir = tempfile::tempdir().unwrap();
        let state = fake_state(dir.path(), json!([{ "id": "1", "key": "K-1", "state": "Open" }]));
        let noisy = format!("echo 'starting up'; exec '{}' '{}'", fake_server().display(), state.display());
        let mut client =
            McpClient::connect(&stdio(vec!["/bin/sh".into(), "-c".into(), noisy]), Duration::from_secs(10), |_| None)
                .unwrap();
        assert_eq!(client.call_tool("issue_read", json!({ "id": "K-1" })).unwrap()["isError"], Value::Null);
        let failed = McpClient::connect(
            &stdio(vec!["/bin/sh".into(), "-c".into(), "echo 'no credentials' >&2; exit 3".into()]),
            Duration::from_secs(10),
            |_| None,
        )
        .err()
        .unwrap();
        let McpError::Transport(message) = failed else { panic!("a dead server is a transport error") };
        assert!(message.contains("exited") && message.contains("no credentials"), "{message}");
        let missing = McpClient::connect(&stdio(vec!["/nonexistent/server".into()]), Duration::from_secs(1), |_| None);
        assert!(matches!(missing, Err(McpError::Transport(m)) if m.contains("starting /nonexistent/server failed")));
        assert!(McpClient::connect(&McpServer::default(), Duration::from_secs(1), |_| None).is_err());
    }

    #[test]
    fn stdio_write_to_a_server_that_never_reads_times_out_and_kills_it() {
        let program = fake_server().display().to_string();
        let config = stdio(vec![program.clone(), "--no-read".into()]);
        let mut server = StdioServer::spawn(&program, &config.command[1..], &config).unwrap();
        let started = Instant::now();
        let error = server
            .exchange(
                &json!({"method": "large", "params": "x".repeat(4 * 1024 * 1024)}),
                Some(1),
                Duration::from_millis(300),
            )
            .unwrap_err();
        assert!(matches!(&error, McpError::Transport(m) if m.contains("did not answer")), "{error:?}");
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(!server.child.wait().unwrap().success(), "the timeout must kill the child");
        assert!(server.writes.recv_timeout(Duration::from_secs(2)).unwrap().is_err(), "the writer must unblock");
    }

    #[test]
    fn stdio_large_tool_call_after_initialize_cannot_block_the_worker() {
        let config = stdio(vec![fake_server().display().to_string(), "--stop-reading-after-initialize".into()]);
        let mut client = McpClient::connect(&config, Duration::from_secs(2), |_| None).unwrap();
        client.timeout = Duration::from_millis(300);
        let started = Instant::now();
        let error = client.call_tool("comment", json!({"body": "x".repeat(4 * 1024 * 1024)})).unwrap_err();
        assert!(matches!(&error, McpError::Transport(m) if m.contains("did not answer")), "{error:?}");
        drop(client);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    /// Status, headers and body of a test server's answer.
    type HttpAnswer = (u16, Vec<(&'static str, String)>, String);

    /// One request per connection, answered by `answer` from the JSON-RPC body.
    fn http_server(answer: fn(&Value) -> HttpAnswer) -> (String, Receiver<String>) {
        http_connections(move |body, stream| {
            let (status, headers, text) = answer(body);
            let mut response =
                format!("HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n", text.len());
            for (name, value) in headers {
                response.push_str(&format!("{name}: {value}\r\n"));
            }
            response.push_str("\r\n");
            response.push_str(&text);
            let _ = stream.write_all(response.as_bytes());
        })
    }

    fn http_connections(answer: impl Fn(&Value, &mut TcpStream) + Send + Sync + 'static) -> (String, Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let (tx, requests) = std::sync::mpsc::channel();
        let answer = Arc::new(answer);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let answer = answer.clone();
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut head = String::new();
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                            length = value.trim().parse().unwrap_or(0);
                        }
                        head.push_str(&line);
                    }
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    let body: Value = serde_json::from_slice(&body).unwrap();
                    let _ = tx.send(format!("{head}\n{body}"));
                    answer(&body, &mut stream);
                });
            }
        });
        (url, requests)
    }

    #[test]
    fn http_keeps_the_session_sends_the_token_and_reads_event_streams() {
        let (url, requests) = http_server(|body| match body["method"].as_str().unwrap() {
            "initialize" => (
                200,
                vec![("Content-Type", "application/json".into()), ("Mcp-Session-Id", "session-1".into())],
                json!({ "jsonrpc": "2.0", "id": body["id"], "result": { "protocolVersion": PROTOCOL_VERSION } })
                    .to_string(),
            ),
            "notifications/initialized" => (202, vec![], String::new()),
            _ => (
                200,
                vec![("Content-Type", "text/event-stream".into())],
                format!(
                    "event: message\ndata: {}\n\ndata: {}\n\n",
                    json!({ "jsonrpc": "2.0", "method": "notifications/progress", "params": {} }),
                    json!({ "jsonrpc": "2.0", "id": body["id"], "result": { "structuredContent": { "id": "9" } } })
                ),
            ),
        });
        let server =
            McpServer { url: Some(url.clone()), token_env: Some("TRACKER_TOKEN".into()), ..McpServer::default() };
        assert_eq!(
            McpClient::connect(&server, Duration::from_secs(5), |_| None).err(),
            Some(McpError::Transport("TRACKER_TOKEN is not set".into()))
        );
        let mut client = McpClient::connect(&server, Duration::from_secs(5), |name| {
            (name == "TRACKER_TOKEN").then(|| "s3cret".into())
        })
        .unwrap();
        let result = client.call_tool("get", json!({ "id": "9" })).unwrap();
        assert_eq!(result["structuredContent"]["id"], "9");
        let seen: Vec<String> = requests.try_iter().collect();
        assert_eq!(seen.len(), 3);
        assert!(
            seen.iter()
                .all(|r| r.contains("authorization: Bearer s3cret") || r.contains("Authorization: Bearer s3cret"))
        );
        let call = seen[2].to_ascii_lowercase();
        assert!(
            call.contains("mcp-session-id: session-1") && call.contains("mcp-protocol-version: 2025-06-18"),
            "{call}"
        );
        assert!(!seen[0].to_ascii_lowercase().contains("mcp-session-id"));

        let (url, _) = http_server(|_| (401, vec![], "unauthorized".into()));
        let failed =
            McpClient::connect(&McpServer { url: Some(url), ..McpServer::default() }, Duration::from_secs(5), |_| None);
        assert!(matches!(failed, Err(McpError::Transport(m)) if m.contains("HTTP 401 (check server.token_env)")));
    }

    fn start_event_stream(stream: &mut TcpStream) {
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nMcp-Session-Id: stream-session\r\nConnection: close\r\n\r\n").unwrap();
    }

    fn write_event(stream: &mut TcpStream, value: Value) {
        // Split JSON over data lines and TCP writes; neither is an event boundary.
        let json = serde_json::to_string_pretty(&value).unwrap();
        let data = format!(
            "event: message\r\n{}\r\n\r\n",
            json.lines().map(|line| format!("data: {line}")).collect::<Vec<_>>().join("\r\n")
        );
        for part in data.as_bytes().chunks(17) {
            write!(stream, "{:x}\r\n", part.len()).unwrap();
            stream.write_all(part).unwrap();
            stream.write_all(b"\r\n").unwrap();
        }
    }

    #[test]
    fn http_returns_the_matching_response_while_the_event_stream_stays_open() {
        let streams = Arc::new(Mutex::new(Vec::new()));
        let held = streams.clone();
        let (url, _) = http_connections(move |body, stream| {
            if body.get("id").is_none() {
                stream.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n").unwrap();
                return;
            }
            held.lock().unwrap().push(stream.try_clone().unwrap());
            start_event_stream(stream);
            write_event(stream, json!({"jsonrpc": "2.0", "id": 999, "result": {}}));
            write_event(stream, json!({"jsonrpc": "2.0", "id": body["id"], "result": {"ok": true}}));
            // No terminating chunk: the response really remains open past the deadline.
        });
        let mut client =
            McpClient::connect(&McpServer { url: Some(url), ..McpServer::default() }, Duration::from_secs(2), |_| None)
                .unwrap();
        for _ in 0..2 {
            assert_eq!(client.call_tool("get", json!({})).unwrap(), json!({"ok": true}));
        }
        assert_eq!(streams.lock().unwrap().len(), 3, "all response streams are still open");
    }

    #[test]
    fn http_answers_server_requests_before_the_tool_response_on_the_same_session() {
        let pending = Arc::new(Mutex::new(None::<(TcpStream, Value)>));
        let held = pending.clone();
        let (url, requests) = http_connections(move |body, stream| {
            match body["method"].as_str() {
                Some("initialize") => {
                    let reply = json!({"jsonrpc": "2.0", "id": body["id"], "result": {}}).to_string();
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: stream-session\r\nContent-Length: {}\r\n\r\n{reply}", reply.len()).unwrap();
                }
                Some("notifications/initialized") => {
                    stream.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n").unwrap();
                }
                Some("tools/call") => {
                    *held.lock().unwrap() = Some((stream.try_clone().unwrap(), body["id"].clone()));
                    start_event_stream(stream);
                    // Deliberately reuse our numeric id: a request is not our response.
                    write_event(stream, json!({"jsonrpc": "2.0", "id": body["id"], "method": "ping"}));
                }
                None => {
                    stream.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n").unwrap();
                    let mut pending = held.lock().unwrap();
                    let (original, id) = pending.as_mut().unwrap();
                    if body.get("result").is_some() {
                        assert_eq!(body["id"], *id);
                        assert_eq!(body["result"], json!({}));
                        write_event(
                            original,
                            json!({"jsonrpc": "2.0", "id": "server-unknown", "method": "sampling/createMessage"}),
                        );
                    } else {
                        assert_eq!(body["id"], "server-unknown");
                        assert_eq!(body["error"]["code"], -32601);
                        write_event(original, json!({"jsonrpc": "2.0", "id": id, "result": {"ok": true}}));
                    }
                }
                other => panic!("unexpected request: {other:?}"),
            }
        });
        let mut client = McpClient::connect(
            &McpServer { url: Some(url), token_env: Some("TOKEN".into()), ..McpServer::default() },
            Duration::from_secs(2),
            |_| Some("secret".into()),
        )
        .unwrap();
        assert_eq!(client.call_tool("get", json!({})).unwrap(), json!({"ok": true}));
        let seen: Vec<_> = requests.try_iter().collect();
        assert_eq!(seen.len(), 5, "initialize, initialized, tool call, ping reply, unknown-method reply");
        for request in &seen[1..] {
            let head = request.to_ascii_lowercase();
            assert!(head.contains("mcp-session-id: stream-session"), "{head}");
            assert!(head.contains("mcp-protocol-version: 2025-06-18"), "{head}");
            assert!(head.contains("authorization: bearer secret"), "{head}");
        }
        assert!(pending.lock().unwrap().is_some(), "the original stream remains open");
    }

    #[test]
    fn http_unanswered_streams_and_stalled_server_replies_are_transport_timeouts() {
        for server_request in [false, true] {
            let streams = Arc::new(Mutex::new(Vec::new()));
            let held = streams.clone();
            let (url, _) = http_connections(move |body, stream| {
                held.lock().unwrap().push(stream.try_clone().unwrap());
                if body.get("method").is_some() {
                    start_event_stream(stream);
                    let message = if server_request {
                        json!({"jsonrpc": "2.0", "method": "ping", "id": "server-ping"})
                    } else {
                        json!({"jsonrpc": "2.0", "method": "notifications/progress"})
                    };
                    write_event(stream, message);
                }
                // Keep both the stream and any reply's HTTP request unanswered.
            });
            let started = Instant::now();
            let failed = McpClient::connect(
                &McpServer { url: Some(url), ..McpServer::default() },
                Duration::from_millis(300),
                |_| None,
            );
            assert!(matches!(failed, Err(McpError::Transport(_))));
            assert!(started.elapsed() < Duration::from_secs(2));
            assert_eq!(streams.lock().unwrap().len(), if server_request { 2 } else { 1 });
        }
    }

    #[test]
    fn event_streams_join_multi_line_data() {
        let mut reader = &b"data: a\r\ndata: b\r\n\r\n: comment\nevent: x\ndata:c"[..];
        assert_eq!(read_sse_event(&mut reader).unwrap(), Some("a\nb".into()));
        assert_eq!(read_sse_event(&mut reader).unwrap(), Some("c".into()));
        assert_eq!(read_sse_event(&mut reader).unwrap(), None);
    }
}
