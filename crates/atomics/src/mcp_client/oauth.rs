//! Browser sign-in for MCP servers, per the MCP authorization spec (2025-06-18): protected-resource
//! metadata (RFC 9728), authorization-server metadata (RFC 8414), dynamic client registration
//! (RFC 7591) as a public client, S256 PKCE and a resource indicator (RFC 8707), with the code
//! delivered to a loopback listener (RFC 8252). Nothing here knows a server by name.
//!
//! Tokens and the client id live in the OS keychain only (see `credentials::Store`). They are never
//! logged, printed, put in an error, or written to disk. Nothing in this module opens a browser
//! except `OAuth::sign_in`, which a person asks for.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use atomics_core::tickets::McpServer;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::digest;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use super::{McpError, PROTOCOL_VERSION};
use crate::connections::Browser;
use crate::credentials::Store;

/// A token this close to expiring is refreshed before it is used.
const REFRESH_SKEW: Duration = Duration::from_secs(60);
/// How long the browser has to come back.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);
/// What a failed or refused sign-in says first; the ticket worker's errors start with it too.
pub const SIGN_IN_REQUIRED: &str = "sign-in required";

/// Whether a server is signed in, from what the keychain holds. It never touches the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInStatus {
    SignedIn,
    NeedsSignIn,
    Error(String),
}

/// Whether a ticket worker's error string says the server needs a sign-in.
pub fn is_sign_in_required(error: &str) -> bool {
    error.starts_with(SIGN_IN_REQUIRED)
}

/// The sign-in API: `sign_in`, `sign_out` and `status` for one server, backed by the OS keychain.
#[derive(Clone)]
pub struct OAuth {
    store: Store,
    browser: Browser,
    callback_timeout: Duration,
}

// Never Debug: the store reaches real tokens.
#[derive(Serialize, Deserialize)]
struct Tokens {
    issuer: String,
    client_id: String,
    token_endpoint: String,
    resource: String,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Access {
    token: String,
    /// Unix seconds.
    expires_at: u64,
}

/// A registered client and the loopback redirect it was registered with.
#[derive(Serialize, Deserialize)]
struct Client {
    client_id: String,
    redirect_uri: String,
}

struct AuthServer {
    issuer: String,
    authorization_endpoint: Url,
    token_endpoint: Url,
    registration_endpoint: Option<Url>,
    iss_in_response: bool,
}

impl OAuth {
    pub fn current() -> Self {
        Self::new(crate::relay::Keychain::current(), Browser::current())
    }

    pub fn new(keychain: crate::relay::Keychain, browser: Browser) -> Self {
        Self { store: Store::new(keychain), browser, callback_timeout: CALLBACK_TIMEOUT }
    }

    #[cfg(test)]
    pub(crate) fn with_callback_timeout(mut self, timeout: Duration) -> Self {
        self.callback_timeout = timeout;
        self
    }

    /// Signed in, needs sign-in, or the keychain's error.
    pub fn status(&self, server: &McpServer) -> SignInStatus {
        let Some(key) = server_key(server) else { return SignInStatus::NeedsSignIn };
        match self.load(&key) {
            Ok(Some((tokens, access))) => {
                let fresh = access.as_ref().is_some_and(|a| a.expires_at > unix_now());
                if tokens.refresh_token.is_some() || fresh { SignInStatus::SignedIn } else { SignInStatus::NeedsSignIn }
            }
            Ok(None) => SignInStatus::NeedsSignIn,
            Err(err) => SignInStatus::Error(err.to_string()),
        }
    }

    /// Credentials used by the current sign-in, for the ticket/trace cleaner only.
    /// This never refreshes, opens a browser, or returns keychain errors containing data.
    pub(crate) fn secrets(&self, server: &McpServer) -> Vec<String> {
        server_key(server)
            .and_then(|key| self.load(&key).ok().flatten())
            .map(|(tokens, access)| tokens.refresh_token.into_iter().chain(access.map(|access| access.token)).collect())
            .unwrap_or_default()
    }

    /// Forgets the server's tokens. The registered client stays cached for the next sign-in.
    pub fn sign_out(&self, server: &McpServer) -> Result<(), McpError> {
        let key = server_key(server).ok_or_else(|| McpError::Server("no MCP server url to sign out of".into()))?;
        self.clear(&key)
    }

    /// Opens the system browser, waits for the authorization to come back and stores the tokens.
    pub fn sign_in(&self, server: &McpServer, timeout: Duration) -> Result<(), McpError> {
        self.sign_in_via(server, timeout, &|url| self.browser.open(url))
    }

    pub(crate) fn sign_in_via(
        &self,
        server: &McpServer,
        timeout: Duration,
        open: &dyn Fn(&str) -> anyhow::Result<()>,
    ) -> Result<(), McpError> {
        let url = server.url.as_deref().map(str::trim).filter(|u| !u.is_empty());
        let url = url.ok_or_else(|| McpError::Server("sign-in needs an HTTP server: set server.url".into()))?;
        let key = server_key(server).expect("has a url");
        let resource = parse_secure(url)?;
        let agent = agent(timeout);

        // 1. Discovery.
        let (metadata_url, scope) = protected_resource_metadata_url(&agent, &resource, server.oauth)?;
        let issuer = resource_issuer(&agent, &metadata_url, &resource)?;
        let auth = authorization_server(&agent, &issuer)?;

        // 2. Registration. A client is reused only if the loopback port it was registered with is free.
        let cached = self.read_json::<Client>(&client_slot(&auth.issuer)).map_err(storage)?;
        let reuse_port = cached
            .as_ref()
            .and_then(|c| Url::parse(&c.redirect_uri).ok())
            .and_then(|u| u.port())
            .and_then(|port| TcpListener::bind(("127.0.0.1", port)).ok());
        let (listener, client) = match (reuse_port, cached) {
            (Some(listener), Some(client)) => (listener, client),
            _ => {
                let listener = TcpListener::bind(("127.0.0.1", 0))
                    .map_err(|err| McpError::Transport(format!("cannot listen for the sign-in callback: {err}")))?;
                let port = listener.local_addr().map_err(|e| McpError::Transport(e.to_string()))?.port();
                let redirect_uri = format!("http://127.0.0.1:{port}/callback");
                let client_id = register(&agent, &auth, &redirect_uri)?;
                let client = Client { client_id, redirect_uri };
                self.write_json(&client_slot(&auth.issuer), &client)?;
                (listener, client)
            }
        };

        // 3. Authorization request.
        let verifier = random_token(32)?;
        let state = random_token(32)?;
        let mut authorize = auth.authorization_endpoint.clone();
        {
            let mut query = authorize.query_pairs_mut();
            query
                .append_pair("response_type", "code")
                .append_pair("client_id", &client.client_id)
                .append_pair("redirect_uri", &client.redirect_uri)
                .append_pair("code_challenge", &challenge(&verifier))
                .append_pair("code_challenge_method", "S256")
                .append_pair("state", &state)
                .append_pair("resource", &canonical_resource(&resource));
            if let Some(scope) = &scope {
                query.append_pair("scope", scope);
            }
        }
        open(authorize.as_str()).map_err(|err| McpError::Server(format!("could not open the browser: {err}")))?;
        let code = await_callback(&listener, &state, &auth, self.callback_timeout)?;

        // 4. Code exchange.
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", client.redirect_uri.as_str()),
            ("client_id", client.client_id.as_str()),
            ("code_verifier", verifier.as_str()),
            ("resource", &canonical_resource(&resource)),
        ];
        let grant = token_request(&agent, &auth.token_endpoint, &form).map_err(|err| match err {
            TokenError::Rejected(message) => {
                McpError::Server(format!("the authorization server refused the code: {message}"))
            }
            TokenError::Transport(err) => err,
        })?;
        let tokens = Tokens {
            issuer: auth.issuer.clone(),
            client_id: client.client_id,
            token_endpoint: auth.token_endpoint.to_string(),
            resource: canonical_resource(&resource),
            refresh_token: grant.refresh_token,
        };
        self.save(&key, &tokens, &grant.access)
    }

    fn load(&self, key: &str) -> anyhow::Result<Option<(Tokens, Option<Access>)>> {
        let Some(tokens) = self.read_json::<Tokens>(&slot("tokens", key))? else { return Ok(None) };
        let access = self.read_json::<Access>(&slot("access", key))?;
        Ok(Some((tokens, access)))
    }

    fn save(&self, key: &str, tokens: &Tokens, access: &Access) -> Result<(), McpError> {
        self.write_json(&slot("access", key), access)?;
        self.write_json(&slot("tokens", key), tokens)
    }

    fn clear(&self, key: &str) -> Result<(), McpError> {
        for kind in ["tokens", "access"] {
            self.store.remove_oauth_item(&slot(kind, key)).map_err(storage)?;
        }
        Ok(())
    }

    fn read_json<T: for<'de> Deserialize<'de>>(&self, slot: &str) -> anyhow::Result<Option<T>> {
        let Some(text) = self.store.oauth_item(slot)? else { return Ok(None) };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|_| anyhow::anyhow!("OS credential item is malformed (contents hidden); sign in again"))
    }

    fn write_json<T: Serialize>(&self, slot: &str, value: &T) -> Result<(), McpError> {
        let text = serde_json::to_string(value).map_err(|_| McpError::Server("sign-in item is not storable".into()))?;
        self.store.put_oauth_item(slot, &text).map_err(storage)
    }

    /// The signed-in session for `server`, refreshed if its token is about to expire.
    pub(crate) fn session(&self, server: &McpServer, timeout: Duration) -> Result<Session, McpError> {
        let url = server.url.as_deref().map(str::trim).unwrap_or_default();
        let key = server_key(server).ok_or_else(|| McpError::Server("no MCP server url".into()))?;
        let need = |why: &str| McpError::SignInRequired(format!("{url} {why}"));
        let (tokens, access) =
            self.load(&key).map_err(|e| McpError::Transport(e.to_string()))?.ok_or_else(|| need("needs a sign-in"))?;
        let mut session = Session {
            oauth: self.clone(),
            key,
            tokens,
            access: access.unwrap_or(Access { token: String::new(), expires_at: 0 }),
            agent: agent(timeout),
            url: url.to_string(),
        };
        session.refresh_if_due()?;
        Ok(session)
    }
}

/// A signed-in connection's tokens: the access token in use, and what refreshes it.
pub(crate) struct Session {
    oauth: OAuth,
    key: String,
    tokens: Tokens,
    access: Access,
    agent: ureq::Agent,
    url: String,
}

impl Session {
    pub(crate) fn access_token(&self) -> &str {
        &self.access.token
    }

    /// Refreshes when the access token is missing or about to expire.
    pub(crate) fn refresh_if_due(&mut self) -> Result<(), McpError> {
        let due = self.access.token.is_empty() || self.access.expires_at <= unix_now() + REFRESH_SKEW.as_secs();
        if due { self.refresh() } else { Ok(()) }
    }

    /// Exchanges the refresh token for a new pair, stores it, and uses it. The refresh token may
    /// rotate; the new one is stored before the old one is dropped. A refusal clears the sign-in
    /// and says `SignInRequired`; a network failure leaves it for the next try.
    pub(crate) fn refresh(&mut self) -> Result<(), McpError> {
        let need = |why: &str| McpError::SignInRequired(format!("{} {why}", self.url));
        let Some(refresh_token) = self.tokens.refresh_token.clone() else {
            let _ = self.oauth.clear(&self.key);
            return Err(need("needs a sign-in again: its token expired and the server gave no refresh token"));
        };
        let endpoint = Url::parse(&self.tokens.token_endpoint).map_err(|_| need("needs a sign-in again"))?;
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", self.tokens.client_id.as_str()),
            ("resource", self.tokens.resource.as_str()),
        ];
        match token_request(&self.agent, &endpoint, &form) {
            Ok(grant) => {
                if let Some(rotated) = grant.refresh_token {
                    self.tokens.refresh_token = Some(rotated);
                }
                self.access = grant.access;
                self.oauth.save(&self.key, &self.tokens, &self.access)
            }
            Err(TokenError::Rejected(_)) => {
                let _ = self.oauth.clear(&self.key);
                Err(need("needs a sign-in again: the server refused the saved sign-in"))
            }
            Err(TokenError::Transport(err)) => Err(err),
        }
    }
}

fn storage(err: anyhow::Error) -> McpError {
    McpError::Transport(err.to_string())
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The server's identity in the keychain: its URL.
fn server_key(server: &McpServer) -> Option<String> {
    let url = server.url.as_deref()?.trim();
    (!url.is_empty()).then(|| digest_hex(url))
}

fn slot(kind: &str, key: &str) -> String {
    format!("{kind}-{key}")
}

fn client_slot(issuer: &str) -> String {
    slot("client", &digest_hex(issuer))
}

fn digest_hex(text: &str) -> String {
    digest::digest(&digest::SHA256, text.as_bytes()).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// RFC 7636's S256 challenge.
fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()))
}

fn random_token(bytes: usize) -> Result<String, McpError> {
    let mut buffer = vec![0u8; bytes];
    SystemRandom::new().fill(&mut buffer).map_err(|_| McpError::Server("no secure random numbers".into()))?;
    Ok(URL_SAFE_NO_PAD.encode(buffer))
}

/// The resource indicator (RFC 8707): the server URL without a fragment or trailing slash.
fn canonical_resource(url: &Url) -> String {
    let mut url = url.clone();
    url.set_fragment(None);
    url.as_str().trim_end_matches('/').to_string()
}

fn agent(timeout: Duration) -> ureq::Agent {
    // No redirects: metadata URLs come from the server, and a redirect must not lead elsewhere.
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into()
}

/// A URL that may carry tokens or metadata: https, or http only to this machine.
fn parse_secure(text: &str) -> Result<Url, McpError> {
    let url = Url::parse(text).map_err(|_| McpError::Server(format!("{text} is not a URL")))?;
    let local = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(name)) => name == "localhost",
        None => false,
    };
    if url.scheme() == "https" || (url.scheme() == "http" && local) {
        Ok(url)
    } else {
        Err(McpError::Server(format!("{url} is not https: refusing to sign in over it")))
    }
}

struct Reply {
    status: u16,
    authenticate: Vec<String>,
    body: String,
}

fn reply(response: Result<ureq::http::Response<ureq::Body>, ureq::Error>, what: &str) -> Result<Reply, McpError> {
    let mut response = response.map_err(|err| McpError::Transport(format!("{what} failed: {err}")))?;
    let authenticate = response
        .headers()
        .get_all("www-authenticate")
        .iter()
        .filter_map(|v| v.to_str().ok().map(str::to_string))
        .collect();
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_string()
        .map_err(|err| McpError::Transport(format!("reading {what} failed: {err}")))?;
    Ok(Reply { status, authenticate, body })
}

fn get_json(agent: &ureq::Agent, url: &Url, what: &str) -> Result<Option<Value>, McpError> {
    let r = reply(agent.get(url.as_str()).header("Accept", "application/json").call(), what)?;
    Ok((r.status == 200).then(|| serde_json::from_str(&r.body).ok()).flatten())
}

/// RFC 9728: where the server's protected-resource metadata is. From the 401's `resource_metadata`,
/// or the well-known URLs when the server was configured `oauth = true` and says nothing.
fn protected_resource_metadata_url(
    agent: &ureq::Agent,
    resource: &Url,
    configured: bool,
) -> Result<(Url, Option<String>), McpError> {
    let probe = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": PROTOCOL_VERSION, "capabilities": {},
        "clientInfo": { "name": "atomics", "version": env!("CARGO_PKG_VERSION") } } });
    let response = agent
        .post(resource.as_str())
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .send(probe.to_string());
    let probed = reply(response, &format!("request to {resource}"))?;
    if probed.status == 401 {
        let challenges = &probed.authenticate;
        let scope = challenges.iter().find_map(|c| auth_param(c, "scope"));
        if let Some(found) = challenges.iter().find_map(|c| auth_param(c, "resource_metadata")) {
            return Ok((parse_secure(&found)?, scope));
        }
    } else if !configured {
        return Err(McpError::Server(format!("{resource} did not ask for a sign-in (HTTP {})", probed.status)));
    }
    // Well-known fallbacks, path-inserted first (RFC 9728 §3.1).
    let mut candidates = Vec::new();
    let origin = resource.origin().ascii_serialization();
    let path = resource.path().trim_end_matches('/');
    if !path.is_empty() {
        candidates.push(format!("{origin}/.well-known/oauth-protected-resource{path}"));
    }
    candidates.push(format!("{origin}/.well-known/oauth-protected-resource"));
    for candidate in candidates {
        let url = Url::parse(&candidate).expect("built from a parsed url");
        if get_json(agent, &url, "protected-resource metadata")?.is_some() {
            return Ok((url, None));
        }
    }
    Err(McpError::Server(format!("{resource} does not publish OAuth protected-resource metadata")))
}

/// Reads the resource metadata and returns `authorization_servers[0]`.
fn resource_issuer(agent: &ureq::Agent, metadata: &Url, resource: &Url) -> Result<String, McpError> {
    let value = get_json(agent, metadata, "protected-resource metadata")?
        .ok_or_else(|| McpError::Server(format!("{metadata} did not return protected-resource metadata")))?;
    // RFC 9728 §3.3: the metadata must describe the resource it was asked for.
    let declared = value["resource"].as_str().and_then(|r| Url::parse(r).ok());
    if declared.as_ref().map(canonical_resource) != Some(canonical_resource(resource)) {
        return Err(McpError::Server(format!("the protected-resource metadata at {metadata} is for another resource")));
    }
    let issuer = value["authorization_servers"]
        .as_array()
        .and_then(|servers| servers.first())
        .and_then(Value::as_str)
        .ok_or_else(|| McpError::Server("the protected-resource metadata names no authorization server".into()))?;
    parse_secure(issuer)?;
    Ok(issuer.to_string())
}

/// RFC 8414 metadata for `issuer`, falling back to OpenID Connect discovery; the issuer must match.
fn authorization_server(agent: &ureq::Agent, issuer: &str) -> Result<AuthServer, McpError> {
    let base = parse_secure(issuer)?;
    let origin = base.origin().ascii_serialization();
    let path = base.path().trim_end_matches('/');
    let candidates = [
        format!("{origin}/.well-known/oauth-authorization-server{path}"),
        format!("{origin}/.well-known/openid-configuration{path}"),
        format!("{origin}{path}/.well-known/openid-configuration"),
    ];
    for candidate in candidates {
        let url = Url::parse(&candidate).map_err(|_| McpError::Server("bad issuer".into()))?;
        let Some(meta) = get_json(agent, &url, "authorization-server metadata")? else { continue };
        let declared = meta["issuer"].as_str().unwrap_or_default();
        if declared.trim_end_matches('/') != issuer.trim_end_matches('/') {
            return Err(McpError::Server(format!(
                "authorization server metadata at {url} is for a different issuer than {issuer}"
            )));
        }
        let endpoint = |name: &str| -> Result<Url, McpError> {
            parse_secure(meta[name].as_str().unwrap_or_default())
                .map_err(|_| McpError::Server(format!("authorization server {issuer} has no usable {name}")))
        };
        if let Some(methods) = meta["code_challenge_methods_supported"].as_array()
            && !methods.iter().any(|m| m == "S256")
        {
            return Err(McpError::Server(format!("authorization server {issuer} does not support S256 PKCE")));
        }
        return Ok(AuthServer {
            issuer: declared.to_string(),
            authorization_endpoint: endpoint("authorization_endpoint")?,
            token_endpoint: endpoint("token_endpoint")?,
            registration_endpoint: meta["registration_endpoint"].as_str().and_then(|u| parse_secure(u).ok()),
            iss_in_response: meta["authorization_response_iss_parameter_supported"].as_bool() == Some(true),
        });
    }
    Err(McpError::Server(format!("{issuer} does not publish authorization server metadata")))
}

/// RFC 7591 registration of a public client with this redirect.
fn register(agent: &ureq::Agent, auth: &AuthServer, redirect_uri: &str) -> Result<String, McpError> {
    let endpoint = auth
        .registration_endpoint
        .as_ref()
        .ok_or_else(|| McpError::Server(format!("{} does not support dynamic client registration", auth.issuer)))?;
    let body = json!({
        "client_name": "ATOMICS",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let r = reply(
        agent
            .post(endpoint.as_str())
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .send(body.to_string()),
        "client registration",
    )?;
    let value: Value = serde_json::from_str(&r.body).unwrap_or(Value::Null);
    match value["client_id"].as_str().filter(|_| matches!(r.status, 200 | 201)) {
        Some(id) if !id.is_empty() => Ok(id.to_string()),
        _ => Err(McpError::Server(format!("{} refused client registration (HTTP {})", auth.issuer, r.status))),
    }
}

/// Takes one callback on the loopback listener: the code, after checking `state` and `iss`.
fn await_callback(
    listener: &TcpListener,
    state: &str,
    auth: &AuthServer,
    timeout: Duration,
) -> Result<String, McpError> {
    listener.set_nonblocking(true).map_err(|e| McpError::Transport(e.to_string()))?;
    let deadline = Instant::now() + timeout;
    loop {
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(McpError::Server("sign-in timed out: the browser did not come back".into()));
                }
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            Err(err) => return Err(McpError::Transport(format!("sign-in callback failed: {err}"))),
        };
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let Some(target) = request_target(&mut stream) else { continue };
        let Ok(url) = Url::parse(&format!("http://127.0.0.1{target}")) else { continue };
        if url.path() != "/callback" {
            let _ = page(&mut stream, "404 Not Found", "Not found.");
            continue;
        }
        let param = |name: &str| url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned());
        let verdict = (|| {
            if param("state").as_deref() != Some(state) {
                return Err("the sign-in answer did not match this request (state mismatch)".to_string());
            }
            match param("iss") {
                Some(iss) if iss.trim_end_matches('/') != auth.issuer.trim_end_matches('/') => {
                    return Err("the sign-in answer came from another authorization server (issuer mismatch)".into());
                }
                None if auth.iss_in_response => {
                    return Err("the sign-in answer did not name its issuer (issuer mismatch)".into());
                }
                _ => {}
            }
            if let Some(error) = param("error") {
                let error: String = error.chars().filter(|c| c.is_ascii_graphic()).take(60).collect();
                return Err(format!("the authorization server answered {error}"));
            }
            param("code").filter(|c| !c.is_empty()).ok_or_else(|| "the sign-in answer had no code".to_string())
        })();
        return match verdict {
            Ok(code) => {
                let _ = page(&mut stream, "200 OK", "Signed in. You can close this tab and return to ATOMICS.");
                Ok(code)
            }
            Err(message) => {
                let _ = page(&mut stream, "400 Bad Request", "Sign-in failed. Return to ATOMICS.");
                Err(McpError::Server(message))
            }
        };
    }
}

/// The request target of a GET, with the rest of the head read off.
fn request_target(stream: &mut TcpStream) -> Option<String> {
    let mut reader = BufReader::new(stream.try_clone().ok()?.take(16 * 1024));
    let mut first = String::new();
    reader.read_line(&mut first).ok()?;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }
    let mut parts = first.split_whitespace();
    (parts.next()? == "GET").then(|| parts.next().map(str::to_string)).flatten()
}

fn page(stream: &mut TcpStream, status: &str, text: &str) -> std::io::Result<()> {
    let body = format!("<!doctype html><title>ATOMICS</title><p>{text}</p>");
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

struct Grant {
    access: Access,
    refresh_token: Option<String>,
}

enum TokenError {
    /// The authorization server said no (a 4xx with an OAuth error).
    Rejected(String),
    Transport(McpError),
}

fn token_request(agent: &ureq::Agent, endpoint: &Url, form: &[(&str, &str)]) -> Result<Grant, TokenError> {
    let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(form).finish();
    let r = reply(
        agent
            .post(endpoint.as_str())
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .send(body),
        "token request",
    )
    .map_err(TokenError::Transport)?;
    let value: Value = serde_json::from_str(&r.body).unwrap_or(Value::Null);
    if r.status >= 500 {
        return Err(TokenError::Transport(McpError::Transport(format!(
            "the token endpoint answered HTTP {}",
            r.status
        ))));
    }
    let access = value["access_token"].as_str().filter(|t| !t.is_empty());
    let bearer = value["token_type"].as_str().is_none_or(|t| t.eq_ignore_ascii_case("bearer"));
    let (true, Some(access), true) = (r.status == 200, access, bearer) else {
        // Only the standard error code is repeated: the description is the server's free text.
        let code: String =
            value["error"].as_str().unwrap_or("no token").chars().filter(|c| c.is_ascii_graphic()).take(60).collect();
        return Err(TokenError::Rejected(format!("{code} (HTTP {})", r.status)));
    };
    let lifetime = value["expires_in"].as_u64().unwrap_or(3600);
    Ok(Grant {
        access: Access { token: access.to_string(), expires_at: unix_now() + lifetime },
        refresh_token: value["refresh_token"].as_str().filter(|t| !t.is_empty()).map(str::to_string),
    })
}

/// A parameter of a `Bearer` challenge, such as `resource_metadata="https://…"`.
fn auth_param(challenge: &str, name: &str) -> Option<String> {
    let rest = challenge.trim_start();
    let rest = rest.get(..6).filter(|s| s.eq_ignore_ascii_case("bearer")).map(|_| &rest[6..])?;
    let mut chars = rest.char_indices().peekable();
    loop {
        while chars.next_if(|(_, c)| c.is_whitespace() || *c == ',').is_some() {}
        let start = chars.peek()?.0;
        let mut end = start;
        while let Some((i, c)) = chars.next_if(|(_, c)| *c != '=' && *c != ',' && !c.is_whitespace()) {
            end = i + c.len_utf8();
        }
        let key = &rest[start..end];
        if chars.next_if(|(_, c)| *c == '=').is_none() {
            continue;
        }
        let mut value = String::new();
        if chars.next_if(|(_, c)| *c == '"').is_some() {
            while let Some((_, c)) = chars.next() {
                match c {
                    '\\' => value.extend(chars.next().map(|(_, c)| c)),
                    '"' => break,
                    c => value.push(c),
                }
            }
        } else {
            while let Some((_, c)) = chars.next_if(|(_, c)| *c != ',' && !c.is_whitespace()) {
                value.push(c);
            }
        }
        if key.eq_ignore_ascii_case(name) {
            return Some(value);
        }
    }
}

#[cfg(test)]
mod tests;
