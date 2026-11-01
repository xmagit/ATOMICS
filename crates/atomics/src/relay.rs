//! The host credential relay. A sandboxed agent never holds a real credential: its session gets a
//! placeholder, and it sends its requests for an upstream host to this relay on this machine's
//! loopback, as `http://host.docker.internal:<port>/<host>/<path>` from inside its VM. The relay
//! swaps the placeholder for the credential and forwards the request to `https://<host>/<path>`
//! when the credential allows that host, method and path, then streams the response back. Each
//! request is logged with its session, host, method, path, status and duration: never a header
//! value, query or body.
//!
//! It runs on threads of its own. Its grants sit behind a lock of their own, held only to look one
//! up and never across I/O; ATOMICS's state lock is never taken, so a slow upstream or a stuck
//! client cannot hold up the app.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

pub mod azure;

fn log_path(log: &std::path::Path) -> PathBuf {
    log.with_file_name("credential-usage.json")
}

/// A request's line and headers.
const MAX_HEAD: usize = 64 * 1024;
/// A request's body, such as a long conversation with images.
const MAX_BODY: u64 = 64 * 1024 * 1024;
/// How long a kept-alive connection waits for its next request.
const IDLE: Duration = Duration::from_secs(300);
/// Connections served at once, so a misbehaving client cannot start threads without bound.
const MAX_CONNECTIONS: usize = 256;
/// The log moves to `relay.log.1` past this size.
const MAX_LOG: u64 = 4 * 1024 * 1024;

/// The Anthropic API, which Claude Code talks to.
pub const ANTHROPIC: &str = "api.anthropic.com";

#[derive(Debug, Clone, Copy)]
pub enum Authorization {
    Bearer,
    ApiKey,
}

/// A credential the relay uses for sandboxes, the hosts it may reach and what it may do there.
pub struct Credential {
    /// How errors name it, such as `Claude subscription`.
    pub label: String,
    source: Source,
    authorization: Authorization,
    /// What a placeholder for it starts with, so clients that check a token's shape accept one.
    prefix: &'static str,
    /// Upstream hosts its requests may go to.
    pub hosts: Vec<String>,
    /// What its requests may do there; anything else is refused.
    pub rules: Vec<Rule>,
}

enum Source {
    Static(String),
    Azure(Arc<azure::Azure>),
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("label", &self.label)
            .field("hosts", &self.hosts)
            .field("rules", &self.rules)
            .finish_non_exhaustive()
    }
}

impl Credential {
    /// A Claude subscription token from `claude setup-token`: inference on the Anthropic API, and
    /// nothing on its account or OAuth endpoints.
    pub fn claude(token: String) -> Self {
        Self {
            label: "Claude subscription".into(),
            source: Source::Static(token),
            authorization: Authorization::Bearer,
            prefix: "sk-ant-oat01-atomics-",
            hosts: vec![ANTHROPIC.into()],
            rules: vec![
                Rule::new(ANTHROPIC, "POST", "/v1/messages"),
                Rule::new(ANTHROPIC, "POST", "/v1/messages/count_tokens"),
                Rule::new(ANTHROPIC, "GET", "/v1/models"),
                Rule::new(ANTHROPIC, "GET", "/v1/models/*"),
            ],
        }
    }

    pub fn azure(azure: Arc<azure::Azure>) -> Self {
        Self {
            label: "Azure CLI".into(),
            authorization: Authorization::Bearer,
            hosts: azure.config.resources.keys().cloned().collect(),
            source: Source::Azure(azure),
            prefix: "atomics-azure-",
            rules: Vec::new(),
        }
    }
    pub fn stored(
        label: String,
        token: String,
        hosts: Vec<String>,
        rules: Vec<Rule>,
        authorization: Authorization,
    ) -> Self {
        Self { label, source: Source::Static(token), authorization, prefix: "atomics-", hosts, rules }
    }
}

/// One thing a credential may do: `method` (`*` for any) on `host`, at `path`, or below it when the
/// path ends in `/*`.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub host: String,
    pub method: String,
    pub path: String,
}

impl Rule {
    pub fn new(host: &str, method: &str, path: &str) -> Self {
        Self { host: host.into(), method: method.into(), path: path.into() }
    }

    fn allows(&self, host: &str, method: &str, path: &str) -> bool {
        self.host.eq_ignore_ascii_case(host)
            && (self.method == "*" || self.method == method)
            && match self.path.strip_suffix('*') {
                Some(prefix) => path.len() > prefix.len() && path.starts_with(prefix),
                None => path == self.path,
            }
    }
}

/// A session's placeholder for one credential.
#[derive(Clone)]
struct Grant {
    session: String,
    credential: Arc<Credential>,
    replies: Arc<Mutex<Vec<std::sync::Weak<crate::ipc::ApprovalReply>>>>,
}

impl Grant {
    fn cancel_writes(&self) {
        for reply in self.replies.lock().unwrap().drain(..).filter_map(|reply| reply.upgrade()) {
            reply.answer(atomics_core::hook::HookReply::default());
        }
    }
}

/// The relay: a listener on this machine's loopback and the placeholders it accepts.
pub struct Relay {
    inner: Arc<Inner>,
    port: u16,
    accept: Option<std::thread::JoinHandle<()>>,
}

struct Inner {
    /// Placeholder → grant.
    grants: Mutex<HashMap<String, Grant>>,
    approvals: Mutex<Vec<WriteApproval>>,
    log: Log,
    agent: ureq::Agent,
    /// Where a host is reached instead of `https://<host>`: tests point hosts at local fakes.
    upstreams: HashMap<String, String>,
    stopped: AtomicBool,
    connections: AtomicUsize,
    usage: Mutex<HashMap<String, u64>>,
}

/// One Azure write waiting for the same approval banner as command permissions. The full
/// bounded request is shown for review; it is never written to the audit log.
pub struct WriteApproval {
    pub session: String,
    pub request: atomics_core::approvals::ApprovalRequest,
    pub reply: Arc<crate::ipc::ApprovalReply>,
}

impl Relay {
    /// Listens on 127.0.0.1 at `port` (0: any free port), logging each request to `log`. A host in
    /// `upstreams` is reached at the address given there instead of `https://<host>`.
    pub fn start(port: u16, log: PathBuf, upstreams: HashMap<String, String>) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
        let port = listener.local_addr()?.port();
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            // The client's own headers go upstream as they are, and ureq adds none of its own.
            .user_agent(ureq::config::AutoHeaderValue::None)
            .accept(ureq::config::AutoHeaderValue::None)
            .accept_encoding(ureq::config::AutoHeaderValue::None)
            .timeout_connect(Some(Duration::from_secs(30)))
            // A long non-streaming answer can take minutes before its first byte; a stream has no end in sight.
            .timeout_recv_response(Some(Duration::from_secs(15 * 60)));
        // Local fakes are never reached through this machine's proxy.
        let config = if upstreams.is_empty() { config } else { config.proxy(None) };
        let inner = Arc::new(Inner {
            grants: Mutex::default(),
            approvals: Mutex::default(),
            log: Log { path: log.clone(), write: Mutex::new(()) },
            agent: config.build().into(),
            upstreams,
            stopped: AtomicBool::new(false),
            connections: AtomicUsize::new(0),
            usage: Mutex::new(
                std::fs::read(log_path(&log))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                    .unwrap_or_default(),
            ),
        });
        let accepting = inner.clone();
        let accept =
            std::thread::Builder::new().name("atomics-relay".into()).spawn(move || accepting.accept(listener))?;
        Ok(Self { inner, port, accept: Some(accept) })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// A new placeholder that stands for `credential` in `session`'s requests until it is revoked.
    pub fn grant(&self, session: &str, credential: Arc<Credential>) -> String {
        let random = || uuid::Uuid::new_v4().simple().to_string();
        let placeholder = format!("{}{}{}", credential.prefix, random(), random());
        let grant = Grant { session: session.into(), credential, replies: Arc::default() };
        self.inner.grants.lock().unwrap().insert(placeholder.clone(), grant);
        placeholder
    }

    /// Stops accepting `placeholder`. A request already under way finishes.
    pub fn revoke(&self, placeholder: &str) {
        if let Some(grant) = self.inner.grants.lock().unwrap().remove(placeholder) {
            grant.cancel_writes();
            self.inner.approvals.lock().unwrap().retain(|approval| {
                if approval.session != grant.session {
                    return true;
                }
                approval.reply.answer(atomics_core::hook::HookReply::default());
                false
            });
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.inner.stopped.store(true, Ordering::Release);
        for (_, grant) in self.inner.grants.lock().unwrap().drain() {
            grant.cancel_writes();
        }
        // Wakes the accept loop, which then sees it has stopped.
        let _ = TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, self.port)), Duration::from_secs(1));
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

/// One connection's share of [`Inner::connections`].
struct Slot(Arc<Inner>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Inner {
    fn accept(self: Arc<Self>, listener: TcpListener) {
        for stream in listener.incoming() {
            if self.stopped.load(Ordering::Acquire) {
                return;
            }
            let Ok(stream) = stream else { continue };
            if !stream.peer_addr().is_ok_and(|peer| peer.ip().is_loopback()) {
                continue;
            }
            if self.connections.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
                self.connections.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
            let slot = Slot(self.clone());
            let _ = std::thread::Builder::new().name("atomics-relay-connection".into()).spawn(move || {
                slot.0.serve(stream);
                drop(slot);
            });
        }
    }

    fn serve(&self, stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(IDLE));
        let _ = stream.set_nodelay(true);
        let Ok(read) = stream.try_clone() else { return };
        let mut reader = BufReader::new(read);
        let mut writer = stream;
        // Each request is read and answered in turn, until the client or the relay is done.
        loop {
            if self.stopped.load(Ordering::Acquire) || !self.exchange(&mut reader, &mut writer).unwrap_or(false) {
                break;
            }
        }
        // Closing with a refused request's body unread would reset the connection before the client
        // reads why: stop writing, and let the rest arrive for a moment.
        let _ = writer.shutdown(Shutdown::Write);
        let _ = writer.set_read_timeout(Some(Duration::from_millis(500)));
        let _ = io::copy(&mut reader.take(1 << 20), &mut io::sink());
    }

    /// Serves one request; true when the connection can carry another.
    fn exchange(&self, reader: &mut impl BufRead, writer: &mut impl Write) -> io::Result<bool> {
        let head = match read_head(reader) {
            Ok(Some(head)) => head,
            Ok(None) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                refuse(writer, 400, "malformed request")?;
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        let started = Instant::now();
        let route = Route::parse(&head.target);
        let grant = head.placeholder().and_then(|placeholder| self.grants.lock().unwrap().get(placeholder).cloned());
        let (host, path) = route.as_ref().map_or(("-", "-"), |route| (route.host.as_str(), route.path.as_str()));
        let session = grant.as_ref().map_or("-", |grant| grant.session.as_str());
        let refused = match (&grant, &route) {
            (None, _) => Some((401, "unknown or revoked placeholder")),
            (_, None) => Some((400, "expected /<host>/<path> with a plain path")),
            (Some(grant), Some(route)) => {
                let credential = &grant.credential;
                if !credential.hosts.iter().any(|allowed| allowed.eq_ignore_ascii_case(&route.host)) {
                    Some((403, "this credential may not reach that host"))
                } else if !match &credential.source {
                    Source::Static(_) => {
                        credential.rules.iter().any(|rule| rule.allows(&route.host, &head.method, &route.path))
                    }
                    Source::Azure(_) => matches!(head.method.as_str(), "GET" | "POST" | "PUT" | "PATCH" | "DELETE"),
                } {
                    Some((403, "this credential may not make that request"))
                } else {
                    None
                }
            }
        };
        if let Some((status, reason)) = refused {
            self.log.record(session, host, &head.method, path, status, started.elapsed());
            // The body stays unread, so the connection cannot carry another request.
            refuse(writer, status, reason)?;
            return Ok(false);
        }
        let (Some(grant), Some(route)) = (&grant, &route) else { unreachable!("refused above") };
        if head.header("expect").is_some_and(|value| value.eq_ignore_ascii_case("100-continue")) {
            writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        }
        let body = match read_body(reader, &head) {
            Ok(body) => body,
            Err((status, reason)) => {
                self.log.record(session, host, &head.method, path, status, started.elapsed());
                refuse(writer, status, reason)?;
                return Ok(false);
            }
        };
        if let Source::Azure(azure) = &grant.credential.source
            && !azure.read(host, &head.method, path)
        {
            // A single decision authorizes only this exact buffered request, never a host or a
            // method broadly. A missing app, a denied/expired reply, or an unreviewably large body
            // fails closed. No token is acquired before the decision.
            let approved = self.approve_write(grant, &head, route, &body);
            if !approved || !self.grants.lock().unwrap().contains_key(head.placeholder().unwrap()) {
                self.log.record(session, host, &head.method, path, 403, started.elapsed());
                refuse(writer, 403, "Azure write was not approved")?;
                return Ok(false);
            }
        }
        {
            let mut usage = self.usage.lock().unwrap();
            usage.insert(
                grant.credential.label.clone(),
                SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
            );
            if let Ok(bytes) = serde_json::to_vec(&*usage) {
                let _ = std::fs::write(log_path(&self.log.path), bytes);
            }
        }
        let (status, whole) = self.forward(&head, route, &grant.credential, body, writer);
        self.log.record(session, host, &head.method, path, status, started.elapsed());
        Ok(whole && head.keep_alive())
    }

    fn approve_write(&self, grant: &Grant, head: &Head, route: &Route, body: &[u8]) -> bool {
        if body.len() > 16 * 1024 || head.target.len() > 16 * 1024 {
            return false;
        }
        let Ok(body) = std::str::from_utf8(body) else {
            return false;
        };
        let reply = Arc::new(crate::ipc::ApprovalReply::default());
        let query = route.query.as_deref().map(|q| format!("?{q}")).unwrap_or_default();
        let headers = head
            .headers
            .iter()
            .filter(|(name, _)| forwarded(name))
            .map(|(name, value)| format!("{name}: {value}\n"))
            .collect::<String>();
        let command = format!("{} https://{}{}{query}\n{headers}\n{body}", head.method, route.host, route.path);
        if command.len() > 32 * 1024 {
            return false;
        }
        let request = atomics_core::approvals::ApprovalRequest {
            tool: "Azure REST".into(),
            command: Some(command),
            cwd: PathBuf::new(),
            reason: Some("Allow this Azure request once? It can change Azure resources.".into()),
            input: serde_json::json!({}),
        };
        // Register under the grants lock so revoke cannot miss a write being enqueued.
        {
            let grants = self.grants.lock().unwrap();
            if !head.placeholder().is_some_and(|placeholder| grants.contains_key(placeholder)) {
                return false;
            }
            let mut replies = grant.replies.lock().unwrap();
            replies.retain(|reply| reply.strong_count() > 0);
            replies.push(Arc::downgrade(&reply));
            self.approvals.lock().unwrap().push(WriteApproval {
                session: grant.session.clone(),
                request,
                reply: reply.clone(),
            });
        }
        let answer = reply.wait(Duration::from_secs(300));
        answer
            .stdout
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .is_some_and(|value| value["hookSpecificOutput"]["decision"]["behavior"] == "allow")
    }

    /// Sends the request upstream with the real credential and streams the response back: its
    /// status, and whether the response went out whole.
    fn forward(
        &self,
        head: &Head,
        route: &Route,
        credential: &Credential,
        body: Vec<u8>,
        writer: &mut impl Write,
    ) -> (u16, bool) {
        let base = self.upstreams.get(&route.host).cloned().unwrap_or_else(|| format!("https://{}", route.host));
        let query = route.query.as_deref().map(|query| format!("?{query}")).unwrap_or_default();
        let mut request =
            ureq::http::Request::builder().method(head.method.as_str()).uri(format!("{base}{}{query}", route.path));
        for (name, value) in head.headers.iter().filter(|(name, _)| forwarded(name)) {
            request = request.header(name.as_str(), value.as_str());
        }
        let secret = match &credential.source {
            Source::Static(secret) => secret.clone(),
            Source::Azure(azure) => match azure.token(&route.host) {
                Ok(secret) => secret,
                Err(reason) => {
                    let _ = refuse(writer, 502, reason);
                    return (502, false);
                }
            },
        };
        let authorization = match credential.authorization {
            Authorization::Bearer => format!("Bearer {}", secret),
            Authorization::ApiKey => secret.clone(),
        };
        let request = request.header("authorization", authorization);
        let sent = if body.is_empty() && !matches!(head.method.as_str(), "POST" | "PUT" | "PATCH") {
            request.body(()).map(|request| self.agent.run(request))
        } else {
            request.body(body).map(|request| self.agent.run(request))
        };
        let (status, reason) = match sent {
            Ok(Ok(response)) => return stream(head, response, writer),
            Err(_) => (400, "a header could not be forwarded"),
            Ok(Err(_)) => (502, "the upstream host could not be reached"),
        };
        let _ = refuse(writer, status, reason);
        (status, false)
    }
}

/// Writes `response` to the client, its body as it arrives so server-sent events stream: its
/// status, and whether it went out whole.
fn stream(head: &Head, response: ureq::http::Response<ureq::Body>, writer: &mut impl Write) -> (u16, bool) {
    let status = response.status();
    let bodiless = head.method == "HEAD" || status.as_u16() == 204 || status.as_u16() == 304;
    let mut out = format!("HTTP/1.1 {} {}\r\n", status.as_u16(), status.canonical_reason().unwrap_or("")).into_bytes();
    for (name, value) in response.headers() {
        if hop_by_hop(name.as_str()) || (!bodiless && name.as_str() == "content-length") {
            continue;
        }
        out.extend_from_slice(name.as_str().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    if !bodiless {
        out.extend_from_slice(b"transfer-encoding: chunked\r\n");
    }
    out.extend_from_slice(b"\r\n");
    if writer.write_all(&out).and_then(|()| writer.flush()).is_err() {
        return (status.as_u16(), false);
    }
    if bodiless {
        return (status.as_u16(), true);
    }
    let mut upstream = response.into_body().into_reader();
    let mut buffer = vec![0; 16 * 1024];
    loop {
        let read = match upstream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            // Without its last chunk the client sees the response cut short, as it was.
            Err(_) => return (status.as_u16(), false),
        };
        let mut chunk = format!("{read:x}\r\n").into_bytes();
        chunk.extend_from_slice(&buffer[..read]);
        chunk.extend_from_slice(b"\r\n");
        if writer.write_all(&chunk).and_then(|()| writer.flush()).is_err() {
            return (status.as_u16(), false);
        }
    }
    let ended = writer.write_all(b"0\r\n\r\n").and_then(|()| writer.flush()).is_ok();
    (status.as_u16(), ended)
}

/// Request headers that do not go upstream: hop-by-hop ones, those ureq sets itself, and every
/// credential the client sent, which the relay's own replaces.
fn forwarded(name: &str) -> bool {
    !hop_by_hop(name)
        && ![
            "host",
            "content-length",
            "expect",
            "authorization",
            "x-api-key",
            "proxy-authorization",
            "cookie",
            "x-http-method",
            "x-http-method-override",
            "x-method-override",
        ]
        .iter()
        .any(|dropped| name.eq_ignore_ascii_case(dropped))
}

fn hop_by_hop(name: &str) -> bool {
    ["connection", "keep-alive", "proxy-connection", "te", "trailer", "transfer-encoding", "upgrade"]
        .iter()
        .any(|hop| name.eq_ignore_ascii_case(hop))
}

/// An error in the shape of the Anthropic API's, which clients show as it is; the connection closes.
fn refuse(writer: &mut impl Write, status: u16, reason: &str) -> io::Result<()> {
    let kind = match status {
        401 => "authentication_error",
        403 => "permission_error",
        413 => "request_too_large",
        502 => "api_error",
        _ => "invalid_request_error",
    };
    let body =
        serde_json::json!({ "type": "error", "error": { "type": kind, "message": format!("ATOMICS relay: {reason}") } })
            .to_string();
    let reason_phrase = ureq::http::StatusCode::from_u16(status).ok().and_then(|s| s.canonical_reason()).unwrap_or("");
    write!(
        writer,
        "HTTP/1.1 {status} {reason_phrase}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )?;
    writer.flush()
}

/// A request's line and headers.
#[derive(Debug)]
struct Head {
    method: String,
    target: String,
    http11: bool,
    headers: Vec<(String, String)>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }

    /// The placeholder a client sends as its bearer token or API key.
    fn placeholder(&self) -> Option<&str> {
        let bearer = self.header("authorization").and_then(|value| {
            let (scheme, token) = value.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
        });
        bearer
            .or_else(|| self.header("authorization").filter(|value| !value.chars().any(char::is_whitespace)))
            .or_else(|| self.header("x-api-key").map(str::trim))
            .filter(|token| !token.is_empty())
    }

    fn keep_alive(&self) -> bool {
        let connection = self.header("connection").map(str::to_ascii_lowercase);
        match connection.as_deref() {
            Some(value) if value.split(',').any(|v| v.trim() == "close") => false,
            Some(value) if value.split(',').any(|v| v.trim() == "keep-alive") => true,
            _ => self.http11,
        }
    }
}

/// Reads a request's line and headers; None at the end of the connection.
fn read_head(reader: &mut impl BufRead) -> io::Result<Option<Head>> {
    let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_string());
    let mut read = 0;
    let mut line = |reader: &mut dyn BufRead| -> io::Result<Option<String>> {
        let mut bytes = Vec::new();
        let limit = (MAX_HEAD - read) as u64;
        if Read::take(&mut *reader, limit).read_until(b'\n', &mut bytes)? == 0 {
            return Ok(None);
        }
        read += bytes.len();
        if !bytes.ends_with(b"\n") {
            return Err(invalid("request head too long or cut short"));
        }
        let text = String::from_utf8(bytes).map_err(|_| invalid("request head is not UTF-8"))?;
        Ok(Some(text.trim_end_matches(['\r', '\n']).to_string()))
    };
    // A client may send empty lines between requests.
    let request = loop {
        match line(reader)? {
            None => return Ok(None),
            Some(text) if text.is_empty() => continue,
            Some(text) => break text,
        }
    };
    let mut parts = request.split(' ');
    let (Some(method), Some(target), Some(version), None) = (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid("malformed request line"));
    };
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) || !target.starts_with('/') {
        return Err(invalid("malformed request line"));
    }
    let http11 = match version {
        "HTTP/1.1" => true,
        "HTTP/1.0" => false,
        _ => return Err(invalid("unsupported HTTP version")),
    };
    let mut headers = Vec::new();
    loop {
        let Some(text) = line(reader)? else { return Err(invalid("request head cut short")) };
        if text.is_empty() {
            break;
        }
        let Some((name, value)) = text.split_once(':') else { return Err(invalid("malformed header")) };
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)) {
            return Err(invalid("malformed header"));
        }
        headers.push((name.to_string(), value.trim().to_string()));
    }
    Ok(Some(Head { method: method.into(), target: target.into(), http11, headers }))
}

/// Reads a request's body, by its length or in chunks, up to [`MAX_BODY`].
fn read_body(reader: &mut impl BufRead, head: &Head) -> Result<Vec<u8>, (u16, &'static str)> {
    let malformed = (400, "malformed request body");
    let too_large = (413, "request body too large");
    match (head.header("transfer-encoding"), head.header("content-length")) {
        (Some(_), Some(_)) => Err(malformed),
        (Some(encoding), None) => {
            if !encoding.eq_ignore_ascii_case("chunked") {
                return Err((400, "unsupported transfer encoding"));
            }
            let mut body = Vec::new();
            loop {
                let mut line = String::new();
                reader.by_ref().take(1024).read_line(&mut line).map_err(|_| malformed)?;
                let size = line.trim().split(';').next().unwrap_or_default();
                let size = u64::from_str_radix(size.trim(), 16).map_err(|_| malformed)?;
                if size == 0 {
                    // Trailers, then the empty line that ends the body.
                    loop {
                        let mut trailer = String::new();
                        if reader.by_ref().take(8192).read_line(&mut trailer).map_err(|_| malformed)? == 0 {
                            return Err(malformed);
                        }
                        if trailer.trim().is_empty() {
                            return Ok(body);
                        }
                    }
                }
                if body.len() as u64 + size > MAX_BODY {
                    return Err(too_large);
                }
                let start = body.len();
                body.resize(start + size as usize, 0);
                reader.read_exact(&mut body[start..]).map_err(|_| malformed)?;
                let mut end = [0; 2];
                reader.read_exact(&mut end).map_err(|_| malformed)?;
                if &end != b"\r\n" {
                    return Err(malformed);
                }
            }
        }
        (None, Some(length)) => {
            let length: u64 = length.parse().map_err(|_| malformed)?;
            if length > MAX_BODY {
                return Err(too_large);
            }
            let mut body = vec![0; length as usize];
            reader.read_exact(&mut body).map_err(|_| malformed)?;
            Ok(body)
        }
        (None, None) => Ok(Vec::new()),
    }
}

/// Where a request goes: `/<host>/<path>?<query>`. Paths are plain, without `.` or `..` segments,
/// empty segments or percent-escapes, so a rule's path means the path the upstream host serves.
#[derive(Debug, PartialEq)]
struct Route {
    host: String,
    path: String,
    query: Option<String>,
}

impl Route {
    fn parse(target: &str) -> Option<Self> {
        let (path, query) = match target.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (target, None),
        };
        let rest = path.strip_prefix('/')?;
        let (host, path) = match rest.split_once('/') {
            Some((host, path)) => (host, format!("/{path}")),
            None => (rest, "/".to_string()),
        };
        let host_ok = !host.is_empty()
            && host.len() <= 253
            && host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
            && !host.starts_with(['.', '-']);
        let segments: Vec<&str> = path[1..].split('/').collect();
        let segments_ok = path.len() <= 2048
            && segments.iter().enumerate().all(|(index, segment)| {
                (!segment.is_empty() || index == segments.len() - 1)
                    && *segment != "."
                    && *segment != ".."
                    && segment.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@".contains(&b))
            });
        let query_ok =
            query.is_none_or(|query| query.len() <= 8192 && query.bytes().all(|b| b.is_ascii_graphic() && b != b'#'));
        (host_ok && segments_ok && query_ok).then(|| Self {
            host: host.to_ascii_lowercase(),
            path,
            query: query.map(String::from),
        })
    }
}

/// `relay.log`: one line per request, never a value.
struct Log {
    path: PathBuf,
    write: Mutex<()>,
}

impl Log {
    fn record(&self, session: &str, host: &str, method: &str, path: &str, status: u16, elapsed: Duration) {
        use std::os::unix::fs::OpenOptionsExt;
        let line =
            format!("{} {session} {host} {method} {path} {status} {}ms\n", utc(SystemTime::now()), elapsed.as_millis());
        let _write = self.write.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if std::fs::metadata(&self.path).is_ok_and(|file| file.len() > MAX_LOG) {
            let _ = std::fs::rename(&self.path, self.path.with_extension("log.1"));
        }
        let file = std::fs::File::options().create(true).append(true).mode(0o600).open(&self.path);
        if let Err(error) = file.and_then(|mut file| file.write_all(line.as_bytes())) {
            eprintln!("ATOMICS: writing {}: {error}", self.path.display());
        }
    }
}

/// `time` as `2026-10-30T13:45:00Z`.
pub(crate) fn utc(time: SystemTime) -> String {
    let seconds = time.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let (days, rest) = ((seconds / 86_400) as i64, seconds % 86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted + 2) / 5 + 1;
    let month = if shifted < 10 { shifted + 3 } else { shifted - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rest / 3600, rest % 3600 / 60, rest % 60)
}

/// The OS keychain entry `atomics setup` keeps the Claude subscription token in, from `claude setup-token`.
pub const CLAUDE_SERVICE: &str = "atomics-claude-setup-token";
const ACCOUNT: &str = "atomics";

/// The OS keychain, through its command-line tool: `security` on macOS, `secret-tool` (the Secret
/// Service) on Linux. What it prints is never shown.
#[derive(Debug, Clone)]
pub struct Keychain {
    program: String,
    mac: bool,
}

impl Keychain {
    pub fn current() -> Self {
        let mac = cfg!(target_os = "macos");
        let program = if mac {
            "/usr/bin/security".into()
        } else {
            crate::setup::find_program("secret-tool")
                .map_or_else(|| "/usr/bin/secret-tool".into(), |path| path.to_string_lossy().into_owned())
        };
        Self { program, mac }
    }

    /// The keychain tool at `program`, which speaks macOS's `security` when `mac`, else `secret-tool`.
    pub fn at(program: &std::path::Path, mac: bool) -> Self {
        Self { program: program.to_string_lossy().into_owned(), mac }
    }

    fn lookup_args(&self) -> Vec<&'static str> {
        if self.mac {
            vec!["find-generic-password", "-a", ACCOUNT, "-s", CLAUDE_SERVICE, "-w"]
        } else {
            vec!["lookup", "service", CLAUDE_SERVICE, "account", ACCOUNT]
        }
    }

    /// The Claude subscription token, when one is stored and looks like one. The keychain's tool
    /// can wait for the keychain to unlock: call this off the state lock.
    pub fn claude_token(&self) -> Option<String> {
        crate::credentials::Store::new(self.clone()).token("claude").ok().flatten()
    }

    fn output(&self, args: &[&str]) -> std::result::Result<std::process::Output, crate::sandbox::setup::OutputError> {
        let mut command = std::process::Command::new(&self.program);
        command.args(args);
        for name in ["ATOMICS_SOCKET", "ATOMICS_PANE", "ATOMICS_HOME", "ATOMICS_CONFIG", "SSH_AUTH_SOCK"] {
            command.env_remove(name);
        }
        if let Some(path) = crate::setup::SHELL_PATH.as_ref() {
            command.env("PATH", path);
        }
        crate::sandbox::setup::private_output(&mut command, Duration::from_secs(60))
    }

    pub(crate) fn legacy_token(&self) -> Option<String> {
        let output = self.output(&self.lookup_args()).ok()?;
        let token = std::str::from_utf8(&output.stdout).ok()?.trim();
        (output.status.success() && !token.is_empty()).then(|| token.to_string())
    }

    pub(crate) fn read_catalog(&self) -> Result<Option<String>> {
        match self.read_item("atomics-credentials-v1", ACCOUNT)? {
            Some(text) => Ok(Some(text)),
            // Builds from before the rename to ATOMICS; the next save writes the new item.
            None => self.read_item("imacs-credentials-v1", "imacs"),
        }
    }

    pub(crate) fn read_item(&self, service: &str, account: &str) -> Result<Option<String>> {
        let args = if self.mac {
            vec!["find-generic-password", "-a", account, "-s", service, "-w"]
        } else {
            vec!["lookup", "service", service, "account", account]
        };
        let output =
            self.output(&args).map_err(|_| anyhow::anyhow!("OS credential store unavailable; unlock it and retry"))?;
        if output.status.success() {
            let text = std::str::from_utf8(&output.stdout)
                .map_err(|_| anyhow::anyhow!("OS credential store returned invalid data (contents hidden)"))?;
            Ok(Some(text.trim().to_string()))
        } else if (self.mac && output.status.code() == Some(44))
            || (!self.mac && output.status.code() == Some(1) && output.stderr.iter().all(u8::is_ascii_whitespace))
        {
            Ok(None)
        } else {
            anyhow::bail!("OS credential store unavailable; unlock it and retry")
        }
    }

    pub(crate) fn catalog_lock(&self) -> Result<std::fs::File> {
        let directory = if cfg!(test) {
            std::path::Path::new(&self.program).parent().unwrap().to_path_buf()
        } else {
            atomics_core::paths::atomics_home()
        };
        lock_file(&directory, "credentials.lock").map_err(|_| anyhow::anyhow!("cannot lock credential store"))
    }

    /// Serializes one MCP server's sign-in, refresh and sign-out across every ATOMICS process of this
    /// user. It lives next to the config rather than in an ATOMICS home: sessions with different homes
    /// share the one keychain, and so the one rotating refresh token.
    pub(crate) fn sign_in_lock(&self, key: &str) -> Result<std::fs::File> {
        let directory = if cfg!(test) {
            std::path::Path::new(&self.program).parent().unwrap().to_path_buf()
        } else {
            let config = atomics_core::paths::config_file();
            config.parent().map_or_else(atomics_core::paths::atomics_home, std::path::Path::to_path_buf)
        };
        lock_file(&directory, &format!("mcp-oauth-{key}.lock")).map_err(|_| anyhow::anyhow!("cannot lock the sign-in"))
    }

    pub(crate) fn remove_legacy(&self) -> Result<()> {
        let args = if self.mac {
            vec!["delete-generic-password", "-a", ACCOUNT, "-s", CLAUDE_SERVICE]
        } else {
            vec!["clear", "service", CLAUDE_SERVICE, "account", ACCOUNT]
        };
        let output =
            self.output(&args).map_err(|_| anyhow::anyhow!("credential saved; legacy entry cleanup failed, retry"))?;
        if output.status.success()
            || (self.mac && output.status.code() == Some(44))
            || (!self.mac && output.status.code() == Some(1) && output.stderr.iter().all(u8::is_ascii_whitespace))
        {
            return Ok(());
        }
        anyhow::bail!("credential saved; legacy entry cleanup failed, retry")
    }

    pub(crate) fn write_catalog(&self, encoded: &str) -> Result<()> {
        self.write_item("atomics-credentials-v1", ACCOUNT, "ATOMICS credentials", encoded)
    }

    /// Writes one keychain item and reads it back. `encoded` is hex, so security's interactive
    /// parser only sees letters and digits; the OS keychain supplies encryption and access control.
    pub(crate) fn write_item(&self, service: &str, account: &str, label: &str, encoded: &str) -> Result<()> {
        let (args, input) = if self.mac {
            (vec!["-i".to_string()], format!("add-generic-password -U -a {account} -s {service} -w {encoded}\n"))
        } else {
            (
                ["store", &format!("--label={label}"), "service", service, "account", account]
                    .map(str::to_string)
                    .to_vec(),
                format!("{encoded}\n"),
            )
        };
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        // security.c's MAX_LINE_LEN is 4096, including the terminating NUL. Reject before
        // invoking it: a truncated interactive command could overwrite the current item.
        if self.mac && input.len() >= 4096 {
            anyhow::bail!(
                "credential item exceeds macOS security's 4 KiB input limit; existing credentials are unchanged"
            )
        }
        crate::sandbox::setup::private_command(&self.program, &args, &input)
            .map_err(|_| anyhow::anyhow!("OS credential store write failed; unlock it and retry"))?;
        if self.read_item(service, account)?.as_deref() != Some(encoded) {
            anyhow::bail!("OS credential store could not verify the write")
        }
        Ok(())
    }

    /// Deletes one keychain item; an absent item is fine.
    pub(crate) fn remove_item(&self, service: &str, account: &str) -> Result<()> {
        let args = if self.mac {
            vec!["delete-generic-password", "-a", account, "-s", service]
        } else {
            vec!["clear", "service", service, "account", account]
        };
        let output =
            self.output(&args).map_err(|_| anyhow::anyhow!("OS credential store unavailable; unlock it and retry"))?;
        if output.status.success()
            || (self.mac && output.status.code() == Some(44))
            || (!self.mac && output.status.code() == Some(1) && output.stderr.iter().all(u8::is_ascii_whitespace))
        {
            return Ok(());
        }
        anyhow::bail!("OS credential store unavailable; unlock it and retry")
    }
}

/// An exclusive lock on `directory/name`, waiting for it; closing the file releases it. `File::lock`
/// is `flock` on Unix and `LockFileEx` on Windows, so it holds across processes on every platform.
fn lock_file(directory: &std::path::Path, name: &str) -> std::io::Result<std::fs::File> {
    std::fs::create_dir_all(directory)?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(directory.join(name))?;
    file.lock()?;
    Ok(file)
}

/// Whether `token` looks like a `claude setup-token` token.
pub fn valid_claude_token(token: &str) -> bool {
    token.strip_prefix("sk-ant-oat01-").is_some_and(|rest| {
        !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
}

/// A session's placeholder for the Claude subscription, and the relay's port.
#[derive(Clone, PartialEq)]
pub struct Granted {
    pub port: u16,
    /// Empty when the session has Azure but no Claude subscription.
    pub placeholder: String,
    pub azure: Option<String>,
}

impl std::fmt::Debug for Granted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Granted").field("port", &self.port).finish_non_exhaustive()
    }
}

impl Granted {
    /// Only a random placeholder and a relay address, safe to pass to sbx --env.
    pub fn azure_env(&self) -> Vec<(String, String)> {
        self.azure
            .as_ref()
            .map(|placeholder| {
                vec![
                    (
                        "ATOMICS_AZURE_RELAY_URL".into(),
                        format!("http://{}:{}", atomics_core::isolation::RELAY_HOST, self.port),
                    ),
                    ("ATOMICS_AZURE_BEARER".into(), placeholder.clone()),
                ]
            })
            .unwrap_or_default()
    }
}

/// The relay as the app runs it: started with the first grant, at the port it had last time when
/// that is still free, so sandboxes made before a restart keep their way to it.
pub struct Service {
    home: PathBuf,
    keychain: Keychain,
    edits: Mutex<()>,
    relay: Mutex<Option<Relay>>,
    upstreams: HashMap<String, String>,
    /// Where the port is kept for the next start; None for a relay that lives for one command.
    port_file: Option<PathBuf>,
    azure: Mutex<Option<Arc<azure::Azure>>>,
}

impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Service").field("home", &self.home).finish_non_exhaustive()
    }
}

impl Service {
    pub fn new(home: PathBuf, keychain: Keychain) -> Self {
        let port_file = Some(home.join("relay-port"));
        Self {
            home,
            keychain,
            edits: Mutex::new(()),
            relay: Mutex::default(),
            upstreams: HashMap::new(),
            port_file,
            azure: Mutex::default(),
        }
    }

    /// A relay for one command, such as `atomics sandbox relay-check`, at any free port: it leaves the
    /// running app's port alone.
    pub fn transient(home: PathBuf, keychain: Keychain) -> Self {
        Self { port_file: None, ..Self::new(home, keychain) }
    }

    #[cfg(test)]
    pub fn with_upstreams(home: PathBuf, keychain: Keychain, upstreams: HashMap<String, String>) -> Self {
        Self { upstreams, ..Self::new(home, keychain) }
    }

    #[cfg(test)]
    pub fn with_azure(self, azure: azure::Azure) -> Self {
        *self.azure.lock().unwrap() = Some(Arc::new(azure));
        self
    }

    /// The keychain it reads the Claude subscription from.
    pub fn keychain(&self) -> &Keychain {
        &self.keychain
    }

    /// A new placeholder for `session`'s Claude, standing for the Claude subscription token in the OS
    /// keychain, or None without one. Reads the keychain and may start the relay: call it off the
    /// state lock.
    pub fn grant_claude(&self, session: &str) -> Result<Option<Granted>> {
        self.grant(session, "claude")
    }

    pub fn grant(&self, session: &str, id: &str) -> Result<Option<Granted>> {
        let _edit = self.edits.lock().unwrap();
        let Some(credential) = crate::credentials::Store::new(self.keychain.clone()).credential(id)? else {
            return Ok(None);
        };
        let mut relay = self.relay.lock().unwrap();
        if relay.is_none() {
            *relay = Some(self.start()?);
        }
        let relay = relay.as_ref().unwrap();
        let placeholder = relay.grant(session, Arc::new(credential));
        Ok(Some(Granted { port: relay.port(), placeholder, azure: None }))
    }

    /// Every sandboxed agent can use Azure, independently of its model credentials. Discovery
    /// and token acquisition run off the app state lock; tokens are acquired lazily on requests.
    pub fn grant_session(
        &self,
        session: &str,
        claude: bool,
        config: &atomics_core::isolation::AzureConfig,
    ) -> Result<Option<Granted>> {
        let granted = if claude { self.grant_claude(session)? } else { None };
        if !config.enabled {
            return Ok(granted);
        }
        let mut broker = self.azure.lock().unwrap();
        if broker.as_ref().is_none_or(|azure| &azure.config != config) {
            *broker = azure::Azure::current(config.clone()).map(Arc::new);
        }
        let Some(broker) = broker.as_ref() else {
            return Ok(granted);
        };
        let mut relay = self.relay.lock().unwrap();
        if relay.is_none() {
            *relay = Some(self.start()?);
        }
        let relay = relay.as_ref().unwrap();
        let azure = Some(relay.grant(session, Arc::new(Credential::azure(broker.clone()))));
        Ok(Some(Granted {
            azure,
            ..granted.unwrap_or_else(|| Granted { port: relay.port(), placeholder: String::new(), azure: None })
        }))
    }

    pub fn revoke_azure(&self) {
        self.revoke_credential("Azure CLI");
        self.azure.lock().unwrap().take();
    }

    pub fn take_write_approvals(&self) -> Vec<WriteApproval> {
        self.relay
            .lock()
            .unwrap()
            .as_ref()
            .map(|relay| std::mem::take(&mut *relay.inner.approvals.lock().unwrap()))
            .unwrap_or_default()
    }

    /// Serializes Settings changes with grant preparation. Even an unverified write revokes
    /// the old grants, since the keychain may have accepted the new value before reporting failure.
    pub fn edit(&self, label: &str, update: impl FnOnce(&crate::credentials::Store) -> Result<()>) -> Result<()> {
        let _edit = self.edits.lock().unwrap();
        let result = update(&crate::credentials::Store::new(self.keychain.clone()));
        self.revoke_credential(label);
        result
    }

    /// Only status crosses into Settings; keychain I/O happens on its worker.
    pub fn usage(&self, label: &str) -> (Option<u64>, Vec<String>) {
        let relay = self.relay.lock().unwrap();
        if let Some(relay) = relay.as_ref() {
            let used = relay.inner.usage.lock().unwrap().get(label).copied();
            let mut sessions: Vec<_> = relay
                .inner
                .grants
                .lock()
                .unwrap()
                .values()
                .filter(|grant| grant.credential.label == label)
                .map(|grant| grant.session.clone())
                .collect();
            sessions.sort();
            sessions.dedup();
            (used, sessions)
        } else {
            let used: HashMap<String, u64> = std::fs::read(self.home.join("credential-usage.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default();
            (used.get(label).copied(), Vec::new())
        }
    }

    /// Replacing/removing a credential immediately revokes existing placeholders for it.
    pub fn revoke_credential(&self, label: &str) {
        if let Some(relay) = self.relay.lock().unwrap().as_ref() {
            let placeholders: Vec<_> = relay
                .inner
                .grants
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, grant)| grant.credential.label == label)
                .map(|(placeholder, _)| placeholder.clone())
                .collect();
            for placeholder in placeholders {
                relay.revoke(&placeholder);
            }
        }
    }

    /// How many placeholders the relay accepts now.
    #[cfg(test)]
    pub fn live_grants(&self) -> usize {
        self.relay.lock().unwrap().as_ref().map_or(0, |relay| relay.inner.grants.lock().unwrap().len())
    }

    /// Stops accepting `granted`'s placeholder: its session closed or its agent exited.
    pub fn revoke(&self, granted: &Granted) {
        if let Some(relay) = self.relay.lock().unwrap().as_ref() {
            relay.revoke(&granted.placeholder);
            if let Some(azure) = &granted.azure {
                relay.revoke(azure);
            }
        }
    }

    fn start(&self) -> Result<Relay> {
        let file = self.port_file.as_ref();
        let last = file.and_then(|file| std::fs::read_to_string(file).ok()).and_then(|text| text.trim().parse().ok());
        let log = self.home.join("relay.log");
        std::fs::create_dir_all(&self.home)?;
        let relay = Relay::start(last.unwrap_or(0), log.clone(), self.upstreams.clone())
            .or_else(|_| Relay::start(0, log, self.upstreams.clone()))
            .context("starting ATOMICS's credential relay")?;
        if let Some(file) = file
            && last != Some(relay.port())
        {
            std::fs::write(file, relay.port().to_string()).with_context(|| format!("writing {}", file.display()))?;
        }
        Ok(relay)
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A stand-in upstream: it records each request and answers with server-sent events, holding
    //! the last one back until the test lets it go.
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone, Default)]
    pub struct Seen {
        pub line: String,
        pub headers: Vec<(String, String)>,
        pub body: Vec<u8>,
    }

    impl Seen {
        pub fn header(&self, name: &str) -> Vec<&str> {
            self.headers.iter().filter(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str()).collect()
        }
    }

    pub struct Upstream {
        pub url: String,
        pub seen: Arc<Mutex<Vec<Seen>>>,
        /// Each send lets one held-back event go.
        pub release: Sender<()>,
    }

    /// Sends one request to the relay on a new connection, as a sandbox would: the response's head
    /// and a reader for the rest.
    pub fn send(port: u16, request: &str) -> (String, BufReader<TcpStream>) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(std::time::Duration::from_secs(20))).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut reader = BufReader::new(stream);
        let mut head = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            head.push_str(&line);
            if line == "\r\n" || line.is_empty() {
                return (head, reader);
            }
        }
    }

    /// The next chunk of a chunked response.
    pub fn chunk(reader: &mut impl BufRead) -> String {
        let mut size = String::new();
        reader.read_line(&mut size).unwrap();
        let size = usize::from_str_radix(size.trim(), 16).unwrap();
        let mut data = vec![0; size + 2];
        reader.read_exact(&mut data).unwrap();
        String::from_utf8(data[..size].to_vec()).unwrap()
    }

    pub fn start() -> Upstream {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (release, released) = channel();
        let released = Arc::new(Mutex::new(released));
        let record = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (record, released) = (record.clone(), released.clone());
                std::thread::spawn(move || serve(stream, &record, &released));
            }
        });
        Upstream { url, seen, release }
    }

    fn serve(stream: std::net::TcpStream, record: &Mutex<Vec<Seen>>, released: &Mutex<Receiver<()>>) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        loop {
            let mut seen = Seen::default();
            if reader.read_line(&mut seen.line).unwrap_or(0) == 0 {
                return;
            }
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                let (name, value) = line.split_once(':').unwrap();
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse().unwrap();
                }
                seen.headers.push((name.to_string(), value.trim().to_string()));
            }
            seen.body = vec![0; length];
            reader.read_exact(&mut seen.body).unwrap();
            let path = seen.line.split(' ').nth(1).unwrap_or_default().to_string();
            record.lock().unwrap().push(seen);
            if path.starts_with("/v1/models") {
                let body = r#"{"data":[]}"#;
                write!(
                    writer,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                continue;
            }
            write!(
                writer,
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nanthropic-ratelimit-unified-status: allowed\r\ntransfer-encoding: chunked\r\n\r\n"
            )
            .unwrap();
            for (index, event) in
                ["event: message_start\ndata: {}\n\n", "event: message_stop\ndata: {}\n\n"].iter().enumerate()
            {
                if index > 0 && released.lock().unwrap().recv().is_err() {
                    return;
                }
                write!(writer, "{:x}\r\n{event}\r\n", event.len()).unwrap();
                writer.flush().unwrap();
            }
            write!(writer, "0\r\n\r\n").unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{chunk, send};
    use super::*;

    const TOKEN: &str = "sk-ant-oat01-REAL-subscription-token";

    fn relay(upstream: &fake::Upstream, log: PathBuf) -> Relay {
        Relay::start(0, log, HashMap::from([(ANTHROPIC.to_string(), upstream.url.clone())])).unwrap()
    }

    fn post(placeholder: &str, path: &str, body: &str) -> String {
        format!(
            "POST {path} HTTP/1.1\r\nhost: host.docker.internal\r\nauthorization: Bearer {placeholder}\r\n\
             anthropic-beta: oauth-2025-04-20,fine-grained-tool-streaming-2025-05-14\r\nx-api-key: proxy-managed\r\n\
             content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    /// This machine's IPv4 addresses other than loopback.
    fn outside_addresses() -> Vec<Ipv4Addr> {
        let mut found = Vec::new();
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        // SAFETY: getifaddrs fills `list`, which is walked read-only and freed once.
        unsafe {
            if libc::getifaddrs(&mut list) != 0 {
                return found;
            }
            let mut entry = list;
            while !entry.is_null() {
                let address = (*entry).ifa_addr;
                if !address.is_null() && i32::from((*address).sa_family) == libc::AF_INET {
                    let address = &*(address as *const libc::sockaddr_in);
                    let ip = Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr));
                    if !ip.is_loopback() {
                        found.push(ip);
                    }
                }
                entry = (*entry).ifa_next;
            }
            libc::freeifaddrs(list);
        }
        found
    }

    #[test]
    fn a_placeholder_becomes_the_credential_upstream_and_events_stream_back_as_they_come() {
        let upstream = fake::start();
        let dir = crate::test_support::tempdir().unwrap();
        let relay = relay(&upstream, dir.path().join("relay.log"));
        assert!(TcpListener::bind(("127.0.0.1", relay.port())).is_err(), "the relay listens on loopback");
        // A VPN's interface can accept any connection itself; what matters is that the relay never answers.
        for address in outside_addresses() {
            let outside = SocketAddr::from((address, relay.port()));
            let Ok(mut stream) = TcpStream::connect_timeout(&outside, Duration::from_secs(2)) else { continue };
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let _ = stream.write_all(b"GET /api.anthropic.com/v1/models HTTP/1.1\r\n\r\n");
            let mut answer = Vec::new();
            let _ = stream.take(4096).read_to_end(&mut answer);
            assert!(!String::from_utf8_lossy(&answer).contains("ATOMICS relay"), "the relay answered on {address}");
        }
        let placeholder = relay.grant("atomics-s1", Arc::new(Credential::claude(TOKEN.into())));
        assert!(placeholder.starts_with("sk-ant-oat01-atomics-") && !placeholder.contains("REAL"));

        let body =
            r#"{"model":"claude-sonnet-5-5","stream":true,"messages":[{"role":"user","content":"secret prompt"}]}"#;
        let (head, mut reader) =
            send(relay.port(), &post(&placeholder, "/api.anthropic.com/v1/messages?beta=true", body));
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
        assert!(
            head.contains("content-type: text/event-stream") && head.contains("transfer-encoding: chunked"),
            "{head}"
        );
        assert!(head.contains("anthropic-ratelimit-unified-status: allowed"), "limits reach the client: {head}");
        // The first event arrives while the upstream still holds back the last one.
        assert_eq!(chunk(&mut reader), "event: message_start\ndata: {}\n\n");
        upstream.release.send(()).unwrap();
        assert_eq!(chunk(&mut reader), "event: message_stop\ndata: {}\n\n");
        assert_eq!(chunk(&mut reader), "");

        let seen = upstream.seen.lock().unwrap()[0].clone();
        assert_eq!(seen.line.trim(), "POST /v1/messages?beta=true HTTP/1.1");
        assert_eq!(seen.header("authorization"), [format!("Bearer {TOKEN}")]);
        assert!(seen.header("x-api-key").is_empty(), "the client's other credentials stay behind");
        assert_eq!(seen.header("anthropic-beta"), ["oauth-2025-04-20,fine-grained-tool-streaming-2025-05-14"]);
        assert_eq!(seen.body, body.as_bytes());

        // The same connection carries the next request.
        let mut stream = reader.into_inner();
        let models = format!("GET /api.anthropic.com/v1/models HTTP/1.1\r\nx-api-key: {placeholder}\r\n\r\n");
        stream.write_all(models.as_bytes()).unwrap();
        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        assert_eq!(status, "HTTP/1.1 200 OK\r\n");
        assert_eq!(upstream.seen.lock().unwrap()[1].header("authorization"), [format!("Bearer {TOKEN}")]);
    }

    #[test]
    fn only_granted_placeholders_reach_only_allowed_hosts_methods_and_paths() {
        let upstream = fake::start();
        let dir = crate::test_support::tempdir().unwrap();
        let relay = relay(&upstream, dir.path().join("relay.log"));
        let placeholder = relay.grant("atomics-s1", Arc::new(Credential::claude(TOKEN.into())));
        let status = |request: String| send(relay.port(), &request).0.lines().next().unwrap().to_string();
        let refused = [
            (post("sk-ant-oat01-atomics-unknown", "/api.anthropic.com/v1/messages", "{}"), "401"),
            (post(TOKEN, "/api.anthropic.com/v1/messages", "{}"), "401"),
            (post(&placeholder, "/evil.example.org/v1/messages", "{}"), "403"),
            (post(&placeholder, "/api.anthropic.com/api/oauth/claude_cli/create_api_key", "{}"), "403"),
            (post(&placeholder, "/api.anthropic.com/v1/messages/batches", "{}"), "403"),
            (post(&placeholder, "/api.anthropic.com/v1/models", "{}"), "403"),
            (post(&placeholder, "/api.anthropic.com/v1/messages/../../api/oauth/profile", "{}"), "400"),
            (post(&placeholder, "/api.anthropic.com/v1//messages", "{}"), "400"),
            (post(&placeholder, "/api.anthropic.com/v1/%6dessages", "{}"), "400"),
            (post(&placeholder, "/v1/messages", "{}"), "403"),
            (format!("GET /api.anthropic.com/v1/models HTTP/1.1\r\nauthorization: Basic {placeholder}\r\n\r\n"), "401"),
            ("BREW /api.anthropic.com/v1/messages HTTP/1.1\r\n\r\n".into(), "401"),
            ("get /x HTTP/1.1\r\n\r\n".into(), "400"),
            (format!("GET http://api.anthropic.com/v1/models HTTP/1.1\r\nx-api-key: {placeholder}\r\n\r\n"), "400"),
        ];
        for (request, expected) in refused {
            let line = status(request.clone());
            assert!(line.starts_with(&format!("HTTP/1.1 {expected} ")), "{request:?} → {line}");
        }
        assert!(upstream.seen.lock().unwrap().is_empty(), "nothing refused went upstream");
        let models = format!("GET /api.anthropic.com/v1/models/claude-x HTTP/1.1\r\nx-api-key: {placeholder}\r\n\r\n");
        assert!(status(models.clone()).starts_with("HTTP/1.1 200 "));

        relay.revoke(&placeholder);
        assert!(status(models).starts_with("HTTP/1.1 401 "), "a revoked placeholder is refused");
        assert_eq!(upstream.seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn the_log_has_session_host_method_path_status_and_time_and_never_a_value() {
        let upstream = fake::start();
        let dir = crate::test_support::tempdir().unwrap();
        let log = dir.path().join("relay.log");
        let relay = relay(&upstream, log.clone());
        let placeholder = relay.grant("atomics-s1", Arc::new(Credential::claude(TOKEN.into())));
        let (_, mut reader) = send(
            relay.port(),
            &post(&placeholder, "/api.anthropic.com/v1/messages?beta=private-query", r#"{"content":"private-body"}"#),
        );
        upstream.release.send(()).unwrap();
        while !chunk(&mut reader).is_empty() {}
        send(relay.port(), &post("sk-ant-oat01-atomics-wrong", "/api.anthropic.com/v1/messages", "{}"));
        send(relay.port(), &post(&placeholder, "/evil.example.org/collect", "{}"));
        // A line is written once its response has gone out.
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
        while std::fs::read_to_string(&log).unwrap_or_default().lines().count() < 3 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let text = std::fs::read_to_string(&log).unwrap();
        let mut lines: Vec<Vec<&str>> = text.lines().map(|line| line.split(' ').collect()).collect();
        assert_eq!(lines.len(), 3, "{text}");
        lines.sort_by_key(|line| line[5]);
        assert_eq!(lines[0][1..6], ["atomics-s1", "api.anthropic.com", "POST", "/v1/messages", "200"]);
        assert_eq!(lines[1][1..6], ["-", "api.anthropic.com", "POST", "/v1/messages", "401"]);
        assert_eq!(lines[2][1..6], ["atomics-s1", "evil.example.org", "POST", "/collect", "403"]);
        assert!(lines.iter().all(|line| line[0].ends_with('Z') && line[6].ends_with("ms")), "{text}");
        for private in [TOKEN, placeholder.as_str(), "private-query", "private-body", "wrong", "oauth-2025"] {
            assert!(!text.contains(private), "{private} in the log: {text}");
        }
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&log).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn bodies_arrive_by_length_or_in_chunks_and_oversized_or_malformed_ones_are_refused() {
        let upstream = fake::start();
        let dir = crate::test_support::tempdir().unwrap();
        let relay = relay(&upstream, dir.path().join("relay.log"));
        let placeholder = relay.grant("atomics-s1", Arc::new(Credential::claude(TOKEN.into())));
        let chunked = format!(
            "POST /api.anthropic.com/v1/messages/count_tokens HTTP/1.1\r\nx-api-key: {placeholder}\r\n\
             expect: 100-continue\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n6;x=y\r\n world\r\n0\r\n\r\n"
        );
        let (head, mut reader) = send(relay.port(), &chunked);
        assert_eq!(head, "HTTP/1.1 100 Continue\r\n\r\n");
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        assert_eq!(status, "HTTP/1.1 200 OK\r\n");
        assert_eq!(upstream.seen.lock().unwrap()[0].body, b"hello world");
        upstream.release.send(()).unwrap();
        for (request, expected) in [
            (
                format!(
                    "POST /api.anthropic.com/v1/messages HTTP/1.1\r\nx-api-key: {placeholder}\r\ncontent-length: {}\r\n\r\n",
                    MAX_BODY + 1
                ),
                "413",
            ),
            (
                format!(
                    "POST /api.anthropic.com/v1/messages HTTP/1.1\r\nx-api-key: {placeholder}\r\ntransfer-encoding: chunked\r\n\r\nzz\r\n"
                ),
                "400",
            ),
            (
                format!(
                    "POST /api.anthropic.com/v1/messages HTTP/1.1\r\nx-api-key: {placeholder}\r\ntransfer-encoding: chunked\r\ncontent-length: 3\r\n\r\n"
                ),
                "400",
            ),
        ] {
            let (head, _) = send(relay.port(), &request);
            assert!(head.starts_with(&format!("HTTP/1.1 {expected} ")), "{head}");
        }
        let long = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(MAX_HEAD));
        assert!(send(relay.port(), &long).0.starts_with("HTTP/1.1 400 "));
        assert_eq!(upstream.seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_unreachable_upstream_is_a_502() {
        let dir = crate::test_support::tempdir().unwrap();
        // Nothing listens on port 1, and no test binds it: a freed port could be another test's server.
        let upstreams = HashMap::from([(ANTHROPIC.to_string(), "http://127.0.0.1:1".to_string())]);
        let relay = Relay::start(0, dir.path().join("relay.log"), upstreams).unwrap();
        let placeholder = relay.grant("atomics-s1", Arc::new(Credential::claude(TOKEN.into())));
        let (head, mut reader) = send(relay.port(), &post(&placeholder, "/api.anthropic.com/v1/messages", "{}"));
        assert!(head.starts_with("HTTP/1.1 502 "), "{head}");
        let mut body = String::new();
        reader.read_to_string(&mut body).unwrap();
        assert!(body.contains("api_error") && !body.contains(TOKEN), "{body}");
    }

    #[test]
    fn routes_are_a_host_and_a_plain_path() {
        let route = Route::parse("/API.anthropic.com/v1/messages?beta=true").unwrap();
        assert_eq!(
            route,
            Route { host: ANTHROPIC.into(), path: "/v1/messages".into(), query: Some("beta=true".into()) }
        );
        assert_eq!(Route::parse("/api.anthropic.com").unwrap().path, "/");
        assert_eq!(Route::parse("/api.anthropic.com/v1/models/").unwrap().path, "/v1/models/");
        for bad in
            ["", "x", "//v1", "/-host/x", "/host/a/./b", "/host/a/../b", "/host/a//b", "/host/%2e%2e", "/ho st/x"]
        {
            assert!(Route::parse(bad).is_none(), "{bad}");
        }
        let rule = Rule::new(ANTHROPIC, "GET", "/v1/models/*");
        assert!(rule.allows(ANTHROPIC, "GET", "/v1/models/claude"));
        assert!(!rule.allows(ANTHROPIC, "GET", "/v1/models/") && !rule.allows(ANTHROPIC, "GET", "/v1/models"));
        assert!(
            !rule.allows(ANTHROPIC, "POST", "/v1/models/claude")
                && !rule.allows("api.example.org", "GET", "/v1/models/x")
        );
        assert!(Rule::new(ANTHROPIC, "*", "/v1/messages").allows(ANTHROPIC, "DELETE", "/v1/messages"));
    }

    #[test]
    fn timestamps_are_utc() {
        assert_eq!(utc(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(utc(UNIX_EPOCH + Duration::from_secs(1_793_367_300)), "2026-10-30T13:35:00Z");
        assert_eq!(utc(UNIX_EPOCH + Duration::from_secs(951_782_400)), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn the_service_grants_only_with_a_token_and_keeps_its_port_across_restarts() {
        let dir = crate::test_support::tempdir().unwrap();
        let upstream = fake::start();
        let keychain = dir.path().join("keychain");
        std::fs::copy(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/keychain"), &keychain)
            .unwrap();
        let home = dir.path().join("home");
        let upstreams = HashMap::from([(ANTHROPIC.to_string(), upstream.url.clone())]);
        for mac in [true, false] {
            let _ = std::fs::remove_file(dir.path().join("keychain-value"));
            let service = Service::with_upstreams(home.clone(), Keychain::at(&keychain, mac), upstreams.clone());
            assert_eq!(service.grant_claude("atomics-s1").unwrap(), None, "no token, no relay");
            assert!(service.relay.lock().unwrap().is_none());
            std::fs::write(dir.path().join("keychain-value"), "not-a-claude-token").unwrap();
            assert_eq!(service.grant_claude("atomics-s1").unwrap(), None);
            std::fs::write(dir.path().join("keychain-value"), format!("{TOKEN}\n")).unwrap();
            let granted = service.grant_claude("atomics-s1").unwrap().unwrap();
            assert_eq!(std::fs::read_to_string(home.join("relay-port")).unwrap(), granted.port.to_string());
            let other = service.grant_claude("atomics-s2").unwrap().unwrap();
            assert_eq!(other.port, granted.port);
            assert_ne!(other.placeholder, granted.placeholder);
            let models = |granted: &Granted| {
                let request = format!(
                    "GET /api.anthropic.com/v1/models HTTP/1.1\r\nx-api-key: {}\r\nconnection: close\r\n\r\n",
                    granted.placeholder
                );
                send(granted.port, &request).0.lines().next().unwrap().to_string()
            };
            assert!(models(&granted).contains(" 200 "));
            service.revoke(&granted);
            assert!(models(&granted).contains(" 401 ") && models(&other).contains(" 200 "));
            drop(service);
            // A restarted ATOMICS listens where its sandboxes were told to go. Other tests' sockets can
            // take a free port at any moment, so a few tries are allowed.
            let restarted = (0..5).find_map(|_| {
                let free = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
                std::fs::write(home.join("relay-port"), free.to_string()).unwrap();
                let service = Service::with_upstreams(home.clone(), Keychain::at(&keychain, mac), upstreams.clone());
                let port = service.grant_claude("atomics-s1").unwrap().unwrap().port;
                (port == free).then_some((service, port))
            });
            let (taken, port) = restarted.expect("a restarted relay listens at its recorded port");
            // Taken meanwhile: another port, written down for the next start.
            let service = Service::with_upstreams(home.clone(), Keychain::at(&keychain, mac), upstreams.clone());
            let moved = service.grant_claude("atomics-s1").unwrap().unwrap().port;
            assert_ne!(moved, port);
            assert_eq!(std::fs::read_to_string(home.join("relay-port")).unwrap(), moved.to_string());
            drop((taken, service));
        }
        let argv = std::fs::read_to_string(dir.path().join("keychain-argv")).unwrap();
        assert!(argv.contains("find-generic-password") && argv.contains("lookup") && !argv.contains(TOKEN));
    }

    #[test]
    fn the_keychain_stores_and_reads_a_token_on_both_systems_without_it_in_argv() {
        let dir = crate::test_support::tempdir().unwrap();
        let program = dir.path().join("keychain");
        std::fs::copy(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/keychain"), &program)
            .unwrap();
        for mac in [true, false] {
            let keychain = Keychain::at(&program, mac);
            crate::credentials::Store::new(keychain.clone()).put("claude", TOKEN, None).unwrap();
            let argv = std::fs::read_to_string(dir.path().join("keychain-argv")).unwrap();
            assert!(!argv.contains(TOKEN));
            assert_eq!(keychain.claude_token().as_deref(), Some(TOKEN));
            std::fs::write(dir.path().join("fail-keychain"), "").unwrap();
            assert_eq!(keychain.claude_token(), None);
            std::fs::remove_file(dir.path().join("fail-keychain")).unwrap();
        }
        for (token, valid) in
            [(TOKEN, true), ("sk-ant-oat01-", false), ("sk-ant-api03-key", false), ("sk-ant-oat01-a b", false)]
        {
            assert_eq!(valid_claude_token(token), valid, "{token}");
        }
    }
}
