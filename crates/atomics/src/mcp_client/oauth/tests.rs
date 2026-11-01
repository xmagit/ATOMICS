//! A fake authorization server and a fake MCP HTTP server, both in-process, and a "browser" that is
//! an HTTP GET to the authorization URL followed by a GET to the redirect it answers with.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use super::*;
use crate::mcp_client::McpClient;

struct Req {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn query(&self) -> HashMap<String, String> {
        let query = self.path.split_once('?').map(|(_, q)| q).unwrap_or_default();
        url::form_urlencoded::parse(query.as_bytes()).into_owned().collect()
    }

    fn form(&self) -> HashMap<String, String> {
        url::form_urlencoded::parse(self.body.as_bytes()).into_owned().collect()
    }
}

struct Resp(u16, Vec<(&'static str, String)>, String);

fn json_resp(status: u16, value: Value) -> Resp {
    Resp(status, vec![("Content-Type", "application/json".into())], value.to_string())
}

fn bind() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (listener, url)
}

fn serve(listener: TcpListener, handler: impl Fn(&Req) -> Resp + Send + Sync + 'static) {
    let handler = Arc::new(handler);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let handler = handler.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let mut parts = first.split_whitespace();
                let (method, path) = (parts.next().unwrap().to_string(), parts.next().unwrap().to_string());
                let (mut headers, mut length) = (Vec::new(), 0);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        let (k, v) = (k.to_ascii_lowercase(), v.trim().to_string());
                        if k == "content-length" {
                            length = v.parse().unwrap_or(0);
                        }
                        headers.push((k, v));
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let request = Req { method, path, headers, body: String::from_utf8(body).unwrap() };
                let Resp(status, headers, body) = handler(&request);
                let mut out = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
                for (k, v) in headers {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                let _ = stream.write_all(format!("{out}\r\n{body}").as_bytes());
            });
        }
    });
}

#[derive(Default)]
pub(crate) struct State {
    pub(crate) registrations: Vec<Value>,
    pub(crate) clients: HashMap<String, String>,
    pub(crate) codes: HashMap<String, (String, String, String)>,
    pub(crate) access: HashSet<String>,
    pub(crate) refresh: HashSet<String>,
    pub(crate) counter: u32,
    pub(crate) expires_in: u64,
    pub(crate) authorize_urls: Vec<String>,
    pub(crate) token_requests: Vec<HashMap<String, String>>,
    pub(crate) list_auth: Vec<String>,
    pub(crate) state_override: Option<String>,
    pub(crate) iss_override: Option<String>,
    pub(crate) metadata_issuer: Option<String>,
    pub(crate) oidc_only: bool,
    pub(crate) no_challenge: bool,
    pub(crate) revoked: Vec<HashMap<String, String>>,
    pub(crate) revoke_status: Option<u16>,
    /// How long the token endpoint takes to answer.
    pub(crate) token_delay: Duration,
}

pub(crate) struct Fake {
    as_url: String,
    pub(crate) mcp_url: String,
    pub(crate) state: Mutex<State>,
    tickets: Option<Mutex<McpClient>>,
}

impl Fake {
    pub(crate) fn start() -> Arc<Fake> {
        Self::start_with(None)
    }

    /// Protect the real stdio ticket fixture with this server's OAuth flow. The demo must use
    /// the sign-in's bearer for its searches and reads, rather than switching servers after login.
    pub(crate) fn start_with_tickets(client: McpClient) -> Arc<Fake> {
        Self::start_with(Some(Mutex::new(client)))
    }

    fn start_with(tickets: Option<Mutex<McpClient>>) -> Arc<Fake> {
        let ((as_listener, as_url), (mcp_listener, mcp_base)) = (bind(), bind());
        let fake = Arc::new(Fake {
            as_url,
            mcp_url: format!("{mcp_base}/mcp"),
            state: Mutex::new(State { expires_in: 3600, ..State::default() }),
            tickets,
        });
        let a = fake.clone();
        serve(as_listener, move |r| a.authorization_server(r));
        let m = fake.clone();
        serve(mcp_listener, move |r| m.mcp(r));
        fake
    }

    pub(crate) fn server(&self) -> McpServer {
        McpServer { url: Some(self.mcp_url.clone()), oauth: true, ..McpServer::default() }
    }

    fn authorization_server(&self, r: &Req) -> Resp {
        if r.path == "/token" {
            let delay = self.state.lock().unwrap().token_delay;
            std::thread::sleep(delay);
        }
        let mut st = self.state.lock().unwrap();
        let path = r.path.split('?').next().unwrap();
        match (r.method.as_str(), path) {
            ("GET", "/.well-known/oauth-authorization-server") | ("GET", "/.well-known/openid-configuration") => {
                if st.oidc_only && path.contains("oauth-authorization-server") {
                    return Resp(404, vec![], String::new());
                }
                json_resp(
                    200,
                    json!({
                        "issuer": st.metadata_issuer.clone().unwrap_or(self.as_url.clone()),
                        "authorization_endpoint": format!("{}/authorize", self.as_url),
                        "token_endpoint": format!("{}/token", self.as_url),
                        "registration_endpoint": format!("{}/register", self.as_url),
                        "revocation_endpoint": format!("{}/revoke", self.as_url),
                        "code_challenge_methods_supported": ["S256"],
                        "authorization_response_iss_parameter_supported": true,
                    }),
                )
            }
            ("POST", "/register") => {
                let body: Value = serde_json::from_str(&r.body).unwrap();
                let redirect = body["redirect_uris"][0].as_str().unwrap().to_string();
                assert_eq!(body["token_endpoint_auth_method"], "none");
                assert!(redirect.starts_with("http://127.0.0.1:") && redirect.ends_with("/callback"), "{redirect}");
                let id = format!("client-{}", st.clients.len() + 1);
                st.clients.insert(id.clone(), redirect);
                st.registrations.push(body);
                json_resp(201, json!({ "client_id": id }))
            }
            ("GET", "/authorize") => {
                let q = r.query();
                let ok = q["response_type"] == "code"
                    && q["code_challenge_method"] == "S256"
                    && q["resource"] == self.mcp_url
                    && st.clients.get(&q["client_id"]) == Some(&q["redirect_uri"]);
                if !ok {
                    return Resp(400, vec![], "bad authorization request".into());
                }
                st.authorize_urls.push(r.path.clone());
                st.counter += 1;
                let code = format!("code-{}", st.counter);
                st.codes.insert(
                    code.clone(),
                    (q["code_challenge"].clone(), q["client_id"].clone(), q["redirect_uri"].clone()),
                );
                let state = st.state_override.clone().unwrap_or(q["state"].clone());
                let iss = st.iss_override.clone().unwrap_or(self.as_url.clone());
                let mut target = Url::parse(&q["redirect_uri"]).unwrap();
                target
                    .query_pairs_mut()
                    .append_pair("code", &code)
                    .append_pair("state", &state)
                    .append_pair("iss", &iss);
                Resp(302, vec![("Location", target.to_string())], String::new())
            }
            ("POST", "/revoke") => {
                let form = r.form();
                st.access.remove(&form["token"]);
                st.refresh.remove(&form["token"]);
                st.revoked.push(form);
                Resp(st.revoke_status.unwrap_or(200), vec![], String::new())
            }
            ("POST", "/token") => {
                let form = r.form();
                st.token_requests.push(form.clone());
                let invalid = |error: &str| json_resp(400, json!({ "error": error }));
                if form.get("resource") != Some(&self.mcp_url) {
                    return invalid("invalid_target");
                }
                match form["grant_type"].as_str() {
                    "authorization_code" => {
                        let Some((challenge, client, redirect)) = st.codes.remove(&form["code"]) else {
                            return invalid("invalid_grant");
                        };
                        let proven = super::challenge(&form["code_verifier"]);
                        if proven != challenge || form["client_id"] != client || form["redirect_uri"] != redirect {
                            return invalid("invalid_grant");
                        }
                    }
                    "refresh_token" => {
                        // Rotation: a refresh token works once.
                        if !st.refresh.remove(&form["refresh_token"]) {
                            return invalid("invalid_grant");
                        }
                    }
                    _ => return invalid("unsupported_grant_type"),
                }
                st.counter += 1;
                let (access, refresh) =
                    (format!("access-token-{}", st.counter), format!("refresh-token-{}", st.counter));
                st.access.insert(access.clone());
                st.refresh.insert(refresh.clone());
                json_resp(
                    200,
                    json!({ "access_token": access, "token_type": "Bearer", "refresh_token": refresh, "expires_in": st.expires_in }),
                )
            }
            _ => Resp(404, vec![], String::new()),
        }
    }

    fn mcp(&self, r: &Req) -> Resp {
        let mut st = self.state.lock().unwrap();
        if r.method == "GET" {
            return if r.path == "/.well-known/oauth-protected-resource/mcp" {
                json_resp(200, json!({ "resource": self.mcp_url, "authorization_servers": [self.as_url] }))
            } else {
                Resp(404, vec![], String::new())
            };
        }
        let token = r.header("authorization").and_then(|v| v.strip_prefix("Bearer ")).unwrap_or_default();
        if !st.access.contains(token) {
            let mut headers = vec![];
            if !st.no_challenge {
                let metadata = self.mcp_url.replace("/mcp", "/.well-known/oauth-protected-resource/mcp");
                headers.push((
                    "WWW-Authenticate",
                    format!("Bearer error=\"invalid_token\", resource_metadata=\"{metadata}\", scope=\"tickets.read\""),
                ));
            }
            return Resp(401, headers, "unauthorized".into());
        }
        let body: Value = serde_json::from_str(&r.body).unwrap();
        if let Some(tickets) = &self.tickets {
            // Authentication above applies to every forwarded request. Serialize calls because the
            // fixture records them in one state file, just as the ticket worker serializes jobs.
            drop(st);
            let mut client = tickets.lock().unwrap();
            let result = match body["method"].as_str().unwrap() {
                "tools/list" => client.request("tools/list", body["params"].clone()),
                "tools/call" => client.request("tools/call", body["params"].clone()),
                "initialize" => Ok(json!({})),
                _ => return Resp(202, vec![], String::new()),
            };
            return match result {
                Ok(result) => json_resp(200, json!({"jsonrpc":"2.0", "id":body["id"], "result":result})),
                Err(error) => json_resp(
                    200,
                    json!({"jsonrpc":"2.0", "id":body["id"],
                    "error":{"code":-32603, "message":error.to_string()}}),
                ),
            };
        }
        match body["method"].as_str().unwrap() {
            "initialize" => json_resp(200, json!({ "jsonrpc": "2.0", "id": body["id"], "result": {} })),
            "tools/list" => {
                st.list_auth.push(r.header("authorization").unwrap().to_string());
                json_resp(
                    200,
                    json!({ "jsonrpc": "2.0", "id": body["id"], "result": { "tools": [{ "name": "search" }] } }),
                )
            }
            "tools/call" => json_resp(
                200,
                json!({ "jsonrpc": "2.0", "id": body["id"], "result": { "content": [{ "type": "text", "text": "{}" }] } }),
            ),
            _ => Resp(202, vec![], String::new()),
        }
    }
}

/// The browser: follow the authorization URL's redirect to the loopback callback.
pub(crate) fn browse(url: &str) {
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0).build().into();
    let response = agent.get(url).call().unwrap();
    if let Some(location) = response.headers().get("location").and_then(|v| v.to_str().ok()) {
        let _ = agent.get(location).call();
    }
}

pub(crate) fn open_in_browser(url: &str) -> anyhow::Result<()> {
    let url = url.to_string();
    std::thread::spawn(move || browse(&url));
    Ok(())
}

pub(crate) fn machine() -> (crate::test_support::TempDir, OAuth) {
    let (dir, keychain, _) = crate::credentials::tests::machine(false);
    let oauth = OAuth::new(keychain, Browser::at("unused-browser")).with_callback_timeout(Duration::from_secs(10));
    (dir, oauth)
}

fn sign_in(oauth: &OAuth, server: &McpServer) -> Result<(), McpError> {
    oauth.sign_in_via(server, Duration::from_secs(10), &open_in_browser)
}

fn items(dir: &std::path::Path, kind: &str) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(&format!("keychain-oauth-{kind}-")))
        .count()
}

fn stored_refresh_token(oauth: &OAuth, server: &McpServer) -> String {
    let key = server_key(server).unwrap();
    oauth.read_json::<Tokens>(&slot("tokens", &key)).unwrap().unwrap().refresh_token.unwrap()
}

#[test]
fn pkce_challenge_is_rfc_7636_s256() {
    assert_eq!(challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    let (a, b) = (random_token(32).unwrap(), random_token(32).unwrap());
    assert_ne!(a, b);
    assert_eq!(a.len(), 43);
}

#[test]
fn challenges_are_parsed_for_their_parameters() {
    let header = r#"Bearer realm="x", error="invalid_token", resource_metadata="https://a.example/.well-known/oauth-protected-resource", scope="a b""#;
    assert_eq!(
        auth_param(header, "resource_metadata").unwrap(),
        "https://a.example/.well-known/oauth-protected-resource"
    );
    assert_eq!(auth_param(header, "scope").unwrap(), "a b");
    assert_eq!(auth_param("bearer resource_metadata=https://x/y", "resource_metadata").unwrap(), "https://x/y");
    assert_eq!(auth_param("Basic realm=\"x\"", "realm"), None);
    assert_eq!(auth_param("Bearer realm=\"x\"", "resource_metadata"), None);
}

#[test]
fn signing_in_discovers_registers_proves_pkce_and_uses_the_bearer() {
    let fake = Fake::start();
    let (dir, oauth) = machine();
    let server = fake.server();
    assert_eq!(oauth.status(&server), SignInStatus::NeedsSignIn);
    // A connection never signs in by itself.
    let unsigned = McpClient::connect_with(&server, Duration::from_secs(5), |_| None, Some(&oauth)).err().unwrap();
    assert!(matches!(&unsigned, McpError::SignInRequired(_)), "{unsigned:?}");
    assert!(is_sign_in_required(&unsigned.to_string()));

    sign_in(&oauth, &server).unwrap();
    assert_eq!(oauth.status(&server), SignInStatus::SignedIn);
    {
        let st = fake.state.lock().unwrap();
        // Registration: one public client with a loopback redirect.
        assert_eq!(st.registrations.len(), 1);
        assert_eq!(st.registrations[0]["grant_types"], json!(["authorization_code", "refresh_token"]));
        // The authorization request: S256, state, resource, and the scope the 401 named.
        let url = Url::parse(&format!("http://x{}", st.authorize_urls[0])).unwrap();
        let q: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["resource"], fake.mcp_url);
        assert_eq!(q["scope"], "tickets.read");
        assert!(q["state"].len() >= 43);
        // The token endpoint verified the verifier against the challenge (it refuses otherwise).
        assert_eq!(st.token_requests[0]["grant_type"], "authorization_code");
        assert_eq!(st.token_requests[0]["resource"], fake.mcp_url);
        assert_eq!(st.token_requests[0]["code_verifier"].len(), 43);
    }

    let mut client = McpClient::connect_with(&server, Duration::from_secs(5), |_| None, Some(&oauth)).unwrap();
    assert_eq!(client.tool_names().unwrap(), ["search"]);
    let token = client.oauth_token().unwrap().to_string();
    assert_eq!(fake.state.lock().unwrap().list_auth, [format!("Bearer {token}")]);

    // Tokens are in the keychain only: hex-encoded items, nothing readable in the tool's arguments.
    assert_eq!((items(dir.path(), "tokens"), items(dir.path(), "access"), items(dir.path(), "client")), (1, 1, 1));
    let argv = std::fs::read_to_string(dir.path().join("keychain-argv")).unwrap();
    assert!(!argv.contains(&token));
    for entry in std::fs::read_dir(dir.path()).unwrap().flatten() {
        if entry.file_name().to_string_lossy().starts_with("keychain-oauth-") {
            assert!(!std::fs::read_to_string(entry.path()).unwrap().contains(&token));
        }
    }

    // Signing out removes the items; the registered client stays for the next sign-in.
    oauth.sign_out(&server).unwrap();
    assert_eq!((items(dir.path(), "tokens"), items(dir.path(), "access"), items(dir.path(), "client")), (0, 0, 1));
    assert_eq!(oauth.status(&server), SignInStatus::NeedsSignIn);
    oauth.sign_out(&server).unwrap();
    assert!(McpClient::connect_with(&server, Duration::from_secs(5), |_| None, Some(&oauth)).is_err());
    sign_in(&oauth, &server).unwrap();
    assert_eq!(fake.state.lock().unwrap().registrations.len(), 1, "the client_id was cached");
    assert_eq!(oauth.status(&server), SignInStatus::SignedIn);
}

#[test]
fn discovery_falls_back_to_well_known_urls_and_openid_configuration() {
    let fake = Fake::start();
    {
        let mut st = fake.state.lock().unwrap();
        st.no_challenge = true;
        st.oidc_only = true;
    }
    let (_dir, oauth) = machine();
    sign_in(&oauth, &fake.server()).unwrap();
    assert_eq!(oauth.status(&fake.server()), SignInStatus::SignedIn);
}

#[test]
fn an_unconfigured_server_that_asks_for_a_sign_in_says_so() {
    let fake = Fake::start();
    let plain = McpServer { oauth: false, ..fake.server() };
    let error = McpClient::connect(&plain, Duration::from_secs(5), |_| None).err().unwrap();
    assert!(matches!(&error, McpError::SignInRequired(m) if m.contains("oauth = true")), "{error:?}");
}

#[test]
fn a_token_near_expiry_refreshes_with_rotation_and_a_401_refreshes_once() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server();
    // The first token expires inside the refresh skew.
    fake.state.lock().unwrap().expires_in = 30;
    sign_in(&oauth, &server).unwrap();
    let first = stored_refresh_token(&oauth, &server);
    fake.state.lock().unwrap().expires_in = 3600;

    let mut client = McpClient::connect_with(&server, Duration::from_secs(5), |_| None, Some(&oauth)).unwrap();
    let second = stored_refresh_token(&oauth, &server);
    assert_ne!(first, second, "the rotated refresh token replaced the old one");
    {
        let st = fake.state.lock().unwrap();
        assert!(!st.refresh.contains(&first), "the old refresh token is spent");
        let refreshes: Vec<_> = st.token_requests.iter().filter(|r| r["grant_type"] == "refresh_token").collect();
        assert_eq!(refreshes.len(), 1);
        assert_eq!(refreshes[0]["refresh_token"], first);
        assert_eq!(refreshes[0]["resource"], fake.mcp_url);
    }

    // The server stops accepting the access token: one refresh, then the call goes through.
    fake.state.lock().unwrap().access.clear();
    assert_eq!(client.tool_names().unwrap(), ["search"]);
    assert_ne!(stored_refresh_token(&oauth, &server), second);
    let st = fake.state.lock().unwrap();
    assert_eq!(st.token_requests.iter().filter(|r| r["grant_type"] == "refresh_token").count(), 2);
    assert_eq!(st.list_auth, [format!("Bearer {}", client.oauth_token().unwrap())]);
}

#[test]
fn a_revoked_refresh_token_needs_a_new_sign_in_and_is_not_retried() {
    let fake = Fake::start();
    let (dir, oauth) = machine();
    let server = fake.server();
    sign_in(&oauth, &server).unwrap();
    {
        let mut st = fake.state.lock().unwrap();
        st.access.clear();
        st.refresh.clear();
    }
    let error = McpClient::connect_with(&server, Duration::from_secs(5), |_| None, Some(&oauth)).err().unwrap();
    assert!(matches!(&error, McpError::SignInRequired(_)), "{error:?}");
    assert!(error.to_string().starts_with(SIGN_IN_REQUIRED));
    assert_eq!(oauth.status(&server), SignInStatus::NeedsSignIn);
    assert_eq!((items(dir.path(), "tokens"), items(dir.path(), "access")), (0, 0));
    // Asking again goes straight to SignInRequired without another refresh attempt.
    let again = McpClient::connect_with(&server, Duration::from_secs(5), |_| None, Some(&oauth)).err().unwrap();
    assert!(matches!(again, McpError::SignInRequired(_)));
    let refreshes =
        fake.state.lock().unwrap().token_requests.iter().filter(|r| r["grant_type"] == "refresh_token").count();
    assert_eq!(refreshes, 1);
}

#[test]
fn a_state_mismatch_an_issuer_mismatch_and_foreign_metadata_are_rejected() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server();

    // A forged state is not this sign-in's answer: it is ignored, and the wait runs out.
    fake.state.lock().unwrap().state_override = Some("forged-state".into());
    let error = sign_in(&oauth.clone().with_callback_timeout(Duration::from_millis(500)), &server).unwrap_err();
    assert!(
        matches!(&error, McpError::Server(m) if m.contains("timed out") && m.contains("state mismatch")),
        "{error:?}"
    );

    {
        let mut st = fake.state.lock().unwrap();
        st.state_override = None;
        st.iss_override = Some("http://evil.example".into());
    }
    let error = sign_in(&oauth, &server).unwrap_err();
    assert!(matches!(&error, McpError::Server(m) if m.contains("issuer mismatch")), "{error:?}");

    {
        let mut st = fake.state.lock().unwrap();
        st.iss_override = None;
        st.metadata_issuer = Some("http://127.0.0.1:1".into());
    }
    let error = sign_in(&oauth, &server).unwrap_err();
    assert!(matches!(&error, McpError::Server(m) if m.contains("different issuer")), "{error:?}");

    // Nothing was exchanged or stored along the way.
    assert!(fake.state.lock().unwrap().token_requests.is_empty());
    assert_eq!(oauth.status(&server), SignInStatus::NeedsSignIn);
}

#[test]
fn a_stray_callback_and_a_silent_connection_do_not_end_the_sign_in() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server();
    let strays = Arc::new(Mutex::new(Vec::new()));
    let seen = strays.clone();
    let open = move |url: &str| -> anyhow::Result<()> {
        let authorize = Url::parse(url).unwrap();
        let redirect = authorize.query_pairs().find(|(k, _)| k == "redirect_uri").unwrap().1.into_owned();
        let (url, seen) = (url.to_string(), seen.clone());
        std::thread::spawn(move || {
            // A connection that never sends a byte, held open for the whole sign-in.
            let port = Url::parse(&redirect).unwrap().port().unwrap();
            let _silent = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
            for stray in [format!("{redirect}?code=stolen&state=wrong"), format!("{redirect}?code=stolen")] {
                let mut response = agent.get(&stray).call().unwrap();
                let body = response.body_mut().read_to_string().unwrap();
                seen.lock().unwrap().push((response.status().as_u16(), body));
            }
            browse(&url);
        });
        Ok(())
    };
    let started = Instant::now();
    oauth.sign_in_via(&server, Duration::from_secs(10), &open).unwrap();
    assert!(started.elapsed() < Duration::from_secs(8), "the silent connection held the listener");
    assert_eq!(oauth.status(&server), SignInStatus::SignedIn);
    let strays = strays.lock().unwrap();
    assert_eq!(strays.len(), 2);
    assert!(strays.iter().all(|(status, body)| *status == 400 && body.contains("newest sign-in link")), "{strays:?}");
    // Only the real answer's code was exchanged.
    let st = fake.state.lock().unwrap();
    assert!(st.token_requests.iter().all(|r| r["code"] != "stolen"));
}

#[test]
fn concurrent_refreshers_spend_a_rotating_refresh_token_once() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server();
    fake.state.lock().unwrap().expires_in = 30;
    sign_in(&oauth, &server).unwrap();
    let first = stored_refresh_token(&oauth, &server);
    {
        let mut st = fake.state.lock().unwrap();
        st.expires_in = 3600;
        // Without the lock both refreshers would be inside the token request together.
        st.token_delay = Duration::from_millis(300);
    }
    // Two ATOMICS processes: separate clients over the one keychain, each holding its own lock file.
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let refreshers: Vec<_> = (0..2)
        .map(|_| {
            let (oauth, server, barrier) = (oauth.clone(), server.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                oauth.session(&server, Duration::from_secs(5)).map(|session| session.access_token().to_string())
            })
        })
        .collect();
    let tokens: Vec<String> = refreshers.into_iter().map(|t| t.join().unwrap().unwrap()).collect();
    assert_eq!(tokens[0], tokens[1], "the second refresher used the first one's tokens");
    assert_eq!(oauth.status(&server), SignInStatus::SignedIn);
    let st = fake.state.lock().unwrap();
    let refreshes: Vec<_> = st.token_requests.iter().filter(|r| r["grant_type"] == "refresh_token").collect();
    assert_eq!(refreshes.len(), 1, "one refresh token, spent once");
    assert_eq!(refreshes[0]["refresh_token"], first);
    assert!(st.access.contains(&tokens[0]));
    drop(st);
    assert_ne!(stored_refresh_token(&oauth, &server), first);
}

#[test]
fn a_browser_that_never_comes_back_times_out() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let oauth = oauth.with_callback_timeout(Duration::from_millis(300));
    let started = Instant::now();
    let error = oauth.sign_in_via(&fake.server(), Duration::from_secs(5), &|_| Ok(())).unwrap_err();
    assert!(matches!(&error, McpError::Server(m) if m.contains("timed out")), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn metadata_over_plain_http_to_another_host_is_refused() {
    assert!(parse_secure("https://auth.example/x").is_ok());
    assert!(parse_secure("http://127.0.0.1:9/x").is_ok());
    assert!(parse_secure("http://auth.example/x").is_err());
}

#[test]
fn trace_cleaner_reads_access_and_refresh_secrets_without_refreshing() {
    let (_dir, oauth) = machine();
    let server = McpServer { url: Some("https://tickets.example/mcp".into()), ..Default::default() };
    let key = server_key(&server).unwrap();
    oauth
        .save(
            &key,
            &Tokens {
                issuer: "https://tickets.example".into(),
                client_id: "public-client".into(),
                token_endpoint: "https://tickets.example/token".into(),
                resource: server.url.clone().unwrap(),
                refresh_token: Some("refresh-trace-secret".into()),
                revocation_endpoint: None,
            },
            &Access { token: "access-trace-secret".into(), expires_at: 0 },
        )
        .unwrap();
    let secrets = oauth.secrets(&server).unwrap();
    assert_eq!(secrets.len(), 2);
    let cleaner = atomics_core::tickets::Cleaner::new(secrets);
    let clean = cleaner.redacted("access-trace-secret refresh-trace-secret");
    assert!(!clean.contains("access-trace-secret"));
    assert!(!clean.contains("refresh-trace-secret"));
    oauth.sign_out(&server).unwrap();
    assert!(oauth.secrets(&server).unwrap().is_empty());
}

#[test]
fn an_error_without_state_finishes_only_for_the_expected_issuer() {
    let (listener, base) = bind();
    let issuer = "https://auth.example";
    let auth = AuthServer {
        issuer: issuer.into(),
        authorization_endpoint: Url::parse(issuer).unwrap(),
        token_endpoint: Url::parse(issuer).unwrap(),
        registration_endpoint: None,
        revocation_endpoint: None,
        iss_in_response: true,
    };
    let sender = std::thread::spawn(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
        for query in [
            "error=access_denied&state=wrong&iss=https://auth.example",
            "error=access_denied",
            "error=access_denied&iss=https://other.example",
            "code=stolen&iss=https://auth.example",
            "error=access_denied&iss=https://auth.example",
        ] {
            let response = agent.get(format!("{base}/callback?{query}")).call().unwrap();
            assert_eq!(response.status(), 400);
        }
    });
    let cancel = AtomicBool::new(false);
    let (_tx, paste) = std::sync::mpsc::channel();
    let control =
        SignInControl { cancel: &cancel, paste: &paste, notify: &|_| {}, open_browser: false, can_paste: false };
    let error = await_callback(&listener, "expected-state", &auth, Duration::from_secs(10), &control).unwrap_err();
    sender.join().unwrap();
    assert!(
        matches!(&error, McpError::Server(message) if message == "the authorization server answered access_denied"),
        "{error:?}"
    );
}

#[test]
fn trace_secret_fingerprints_detect_other_process_sign_in_and_access_only_rotation() {
    let (dir, oauth) = machine();
    let fake = Fake::start();
    let server = fake.server();
    assert!(oauth.secrets(&server).unwrap().is_empty(), "cache the initially absent sign-in");
    // An independent handle models a process that cannot invalidate this handle's cache.
    let other = OAuth { trace_secrets: Default::default(), ..oauth.clone() };
    sign_in(&other, &server).unwrap();
    let old = oauth.secrets(&server).unwrap();
    assert_eq!(old.len(), 2);
    let argv = dir.path().join("keychain-argv");
    std::fs::write(&argv, "").unwrap();
    assert_eq!(oauth.secrets(&server).unwrap(), old);
    let calls = std::fs::read_to_string(&argv).unwrap();
    assert_eq!(calls.lines().filter(|line| *line == "lookup").count(), 2, "cached reads inspect heads only");
    let key = server_key(&server).unwrap();
    other
        .write_json(
            &slot("access", &key),
            &Access { token: "access-only-rotation".into(), expires_at: unix_now() + 3600 },
        )
        .unwrap();
    let cleaner = atomics_core::tickets::Cleaner::new(oauth.secrets(&server).unwrap());
    assert_eq!(cleaner.redacted("access-only-rotation"), "***");
    other.sign_out(&server).unwrap();
    assert!(oauth.secrets(&server).unwrap().is_empty());
}
