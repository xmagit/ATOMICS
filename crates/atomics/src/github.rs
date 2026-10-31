//! GitHub's REST and GraphQL APIs for the top-bar counter and the controls modal, and Connect
//! GitHub's sign-in (`gh auth token`, else the OAuth device flow). Everything here runs on worker
//! threads. A token lives in `Token`, which never prints; it is read from the credential catalog's
//! `github-app` entry for each job and sent only as the `Authorization` header to its host's API.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use atomics_core::github::{
    Check, CheckState, Details, GithubConfig, MergeMethod, Pull, RepoId, SCOPES, api_urls, is_github_app, valid_host,
};
use serde_json::{Value, json};

use crate::credentials::{AppGrant, Store};

/// A diff larger than this is refused rather than parsed and highlighted.
const MAX_DIFF: u64 = 16 * 1024 * 1024;
const MAX_BODY: u64 = 8 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

/// A GitHub token. Never Debug, Display or Serialize its value.
#[derive(Clone, PartialEq)]
pub struct Token(String);

impl Token {
    /// A token that looks like one of GitHub's: `gho_`, `ghp_`, `ghu_` or `github_pat_`, printable ASCII.
    pub fn new(value: &str) -> Option<Token> {
        let value = value.trim();
        let known = ["gho_", "ghp_", "ghu_", "github_pat_"].iter().any(|prefix| value.starts_with(prefix));
        (known && value.len() <= 8192 && value.bytes().all(|b| b.is_ascii_graphic())).then(|| Token(value.into()))
    }

    pub fn secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<hidden>)")
    }
}

/// Why a call failed. Messages come from GitHub's `message` field, never from a request.
#[derive(Debug, Clone, PartialEq)]
pub enum ApiError {
    /// 401: the token is missing, revoked or expired.
    Unauthorized,
    /// 403 or 429 with rate-limit headers; the fields are seconds from the response.
    RateLimited {
        status: u16,
        remaining: Option<u64>,
        retry_after: Option<u64>,
        reset_in: Option<u64>,
    },
    Status(u16, String),
    Transport(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Unauthorized => f.write_str("GitHub refused the token; connect GitHub again"),
            ApiError::RateLimited { .. } => {
                f.write_str("GitHub's rate limit was reached; waiting before the next poll")
            }
            ApiError::Status(status, message) if message.is_empty() => write!(f, "GitHub answered {status}"),
            ApiError::Status(status, message) => write!(f, "GitHub answered {status}: {message}"),
            ApiError::Transport(error) => write!(f, "GitHub could not be reached: {error}"),
        }
    }
}

impl ApiError {
    /// How long to wait before polling again, when this was a rate limit.
    pub fn backoff(&self, previous: Option<u64>) -> Option<u64> {
        match *self {
            ApiError::RateLimited { status, remaining, retry_after, reset_in } => {
                atomics_core::github::backoff(status, remaining, retry_after, reset_in, previous)
            }
            _ => None,
        }
    }
}

/// A list of open pull requests and its ETag; `None` when unchanged since the ETag sent.
pub type Listed = Option<(Vec<Pull>, Option<String>)>;

struct Reply {
    status: u16,
    etag: Option<String>,
    body: String,
}

/// Where the APIs are: GitHub's own endpoints, or one local base for every host in tests.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Endpoints {
    /// Replaces `https://api.github.com`, `https://<host>/api/v3` and the web host alike.
    pub base: Option<String>,
    /// The `gh` program; `None` looks it up on the user's PATH.
    pub gh: Option<PathBuf>,
}

impl Endpoints {
    pub fn api(&self, host: &str, token: Option<Token>) -> Api {
        match &self.base {
            Some(base) => Api::at(base, token),
            None => Api::new(host, token),
        }
    }
}

/// One host's REST, GraphQL and web endpoints, with the token sent to them.
pub struct Api {
    rest: String,
    graphql: String,
    web: String,
    token: Option<Token>,
    agent: ureq::Agent,
}

impl Api {
    pub fn new(host: &str, token: Option<Token>) -> Api {
        let (rest, graphql) = api_urls(host);
        Api::with(rest, graphql, format!("https://{host}"), token, false)
    }

    /// Every endpoint under `base`, reached directly: the in-process fake.
    pub fn at(base: &str, token: Option<Token>) -> Api {
        let base = base.trim_end_matches('/');
        Api::with(base.into(), format!("{base}/graphql"), base.into(), token, true)
    }

    fn with(rest: String, graphql: String, web: String, token: Option<Token>, local: bool) -> Api {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(TIMEOUT))
            .user_agent(concat!("atomics/", env!("CARGO_PKG_VERSION")));
        // A local fake is never reached through this machine's proxy.
        let config = if local { config.proxy(None) } else { config };
        Api { rest, graphql, web, token, agent: config.build().into() }
    }

    fn send(
        &self,
        method: &str,
        url: &str,
        accept: &str,
        body: Option<(&str, String)>,
        etag: Option<&str>,
        limit: u64,
    ) -> Result<Reply, ApiError> {
        let mut request = ureq::http::Request::builder()
            .method(method)
            .uri(url)
            .header("accept", accept)
            .header("x-github-api-version", "2022-11-28");
        if let Some(token) = &self.token {
            request = request.header("authorization", format!("Bearer {}", token.secret()));
        }
        if let Some(etag) = etag {
            request = request.header("if-none-match", etag);
        }
        let sent = match body {
            Some((content_type, body)) => {
                request.header("content-type", content_type).body(body).map(|request| self.agent.run(request))
            }
            None => request.body(()).map(|request| self.agent.run(request)),
        };
        let mut response = match sent {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => return Err(ApiError::Transport(transport(&error))),
            Err(_) => return Err(ApiError::Transport("the request could not be built".into())),
        };
        let status = response.status().as_u16();
        let header =
            |name: &str| response.headers().get(name).and_then(|value| value.to_str().ok()).map(str::to_string);
        let number = |name: &str| header(name).and_then(|value| value.trim().parse::<u64>().ok());
        let etag = header("etag");
        let remaining = number("x-ratelimit-remaining");
        let retry_after = number("retry-after");
        let reset_in = number("x-ratelimit-reset").map(|reset| reset.saturating_sub(unix_now()));
        let body = if status == 304 {
            String::new()
        } else {
            response
                .body_mut()
                .with_config()
                .limit(limit)
                .read_to_string()
                .map_err(|error| ApiError::Transport(transport(&error)))?
        };
        match status {
            401 => Err(ApiError::Unauthorized),
            403 | 429 if atomics_core::github::backoff(status, remaining, retry_after, reset_in, None).is_some() => {
                Err(ApiError::RateLimited { status, remaining, retry_after, reset_in })
            }
            200..=299 | 304 => Ok(Reply { status, etag, body }),
            _ => Err(ApiError::Status(status, message(&body))),
        }
    }

    fn get_json(&self, path: &str) -> Result<Value, ApiError> {
        let reply =
            self.send("GET", &format!("{}{path}", self.rest), "application/vnd.github+json", None, None, MAX_BODY)?;
        parse(&reply.body)
    }

    /// The signed-in user's login.
    pub fn viewer(&self) -> Result<String, ApiError> {
        let user = self.get_json("/user")?;
        user["login"].as_str().map(str::to_string).ok_or_else(|| ApiError::Status(200, "no login in /user".into()))
    }

    /// The open pull requests, or `None` when nothing changed since `etag` (a 304 costs no rate limit).
    pub fn pulls(&self, repo: &RepoId, etag: Option<&str>) -> Result<Listed, ApiError> {
        let url = format!("{}/repos/{}/pulls?state=open&per_page=100", self.rest, repo.full_name());
        let reply = self.send("GET", &url, "application/vnd.github+json", None, etag, MAX_BODY)?;
        if reply.status == 304 {
            return Ok(None);
        }
        let list = parse(&reply.body)?;
        let pulls = list
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|pull| {
                Some(Pull {
                    number: pull["number"].as_u64()?,
                    title: pull["title"].as_str().unwrap_or_default().into(),
                    url: pull["html_url"].as_str().unwrap_or_default().into(),
                    author: pull["user"]["login"].as_str().unwrap_or_default().into(),
                    draft: pull["draft"].as_bool().unwrap_or(false),
                    head: pull["head"]["ref"].as_str().unwrap_or_default().into(),
                    head_sha: pull["head"]["sha"].as_str().unwrap_or_default().into(),
                    base: pull["base"]["ref"].as_str().unwrap_or_default().into(),
                    requested: pull["requested_reviewers"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|user| user["login"].as_str().map(str::to_string))
                        .collect(),
                })
            })
            .collect();
        Ok(Some((pulls, reply.etag)))
    }

    /// Checks, review state and mergeability of the open pull requests, and the merge methods allowed.
    pub fn details(&self, repo: &RepoId) -> Result<RepoDetails, ApiError> {
        let body = json!({ "query": DETAILS_QUERY, "variables": { "owner": repo.owner, "name": repo.name } });
        let reply = self.send(
            "POST",
            &self.graphql,
            "application/vnd.github.merge-info-preview+json",
            Some(("application/json", body.to_string())),
            None,
            MAX_BODY,
        )?;
        let value = parse(&reply.body)?;
        if let Some(error) = value["errors"].as_array().and_then(|errors| errors.first()) {
            return Err(ApiError::Status(
                200,
                error["message"].as_str().unwrap_or("GraphQL error").chars().take(200).collect(),
            ));
        }
        let repository = &value["data"]["repository"];
        if repository.is_null() {
            return Err(ApiError::Status(404, format!("{repo} was not found")));
        }
        let mut methods = Vec::new();
        for (field, method) in [
            ("squashMergeAllowed", MergeMethod::Squash),
            ("mergeCommitAllowed", MergeMethod::Merge),
            ("rebaseMergeAllowed", MergeMethod::Rebase),
        ] {
            if repository[field].as_bool().unwrap_or(false) {
                methods.push(method);
            }
        }
        let pulls = repository["pullRequests"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|pull| {
                let rollup = &pull["commits"]["nodes"][0]["commit"]["statusCheckRollup"];
                let check_runs = rollup["contexts"]["nodes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|context| {
                        let (name, state) = match context["__typename"].as_str() {
                            Some("StatusContext") => (&context["context"], context["state"].as_str()),
                            _ => (&context["name"], context["conclusion"].as_str().or(context["status"].as_str())),
                        };
                        Check {
                            name: name.as_str().unwrap_or_default().into(),
                            state: state.map(CheckState::from_github).unwrap_or_default(),
                        }
                    })
                    .collect();
                let details = Details {
                    mergeable: pull["mergeable"].as_str().unwrap_or("UNKNOWN").into(),
                    merge_state: pull["mergeStateStatus"].as_str().unwrap_or("UNKNOWN").into(),
                    review: pull["reviewDecision"].as_str().map(str::to_string),
                    checks: rollup["state"].as_str().map(CheckState::from_github).unwrap_or_default(),
                    check_runs,
                    admin: pull["viewerCanMergeAsAdmin"].as_bool().unwrap_or(false),
                };
                Some((pull["number"].as_u64()?, details))
            })
            .collect();
        Ok(RepoDetails {
            methods,
            default_branch: repository["defaultBranchRef"]["name"].as_str().unwrap_or_default().into(),
            pulls,
        })
    }

    /// Merges with `method`, only if the head is still `sha`. Admin merge is this same call: GitHub
    /// accepts it past the branch rules when the viewer may bypass them.
    pub fn merge(&self, repo: &RepoId, number: u64, method: MergeMethod, sha: &str) -> Result<String, ApiError> {
        let url = format!("{}/repos/{}/pulls/{number}/merge", self.rest, repo.full_name());
        let body = json!({ "merge_method": method.api_name(), "sha": sha });
        let reply = self.send(
            "PUT",
            &url,
            "application/vnd.github+json",
            Some(("application/json", body.to_string())),
            None,
            MAX_BODY,
        )?;
        let value = parse(&reply.body)?;
        Ok(value["message"].as_str().unwrap_or("Pull request merged").into())
    }

    /// The pull request's head commit now.
    pub fn head(&self, repo: &RepoId, number: u64) -> Result<String, ApiError> {
        let value = self.get_json(&format!("/repos/{}/pulls/{number}", repo.full_name()))?;
        value["head"]["sha"]
            .as_str()
            .filter(|sha| !sha.is_empty())
            .map(str::to_string)
            .ok_or_else(|| ApiError::Status(200, "no head commit in the pull request".into()))
    }

    /// The pull request's diff against its merge-base, as GitHub shows it.
    pub fn diff(&self, repo: &RepoId, number: u64) -> Result<String, ApiError> {
        let url = format!("{}/repos/{}/pulls/{number}", self.rest, repo.full_name());
        Ok(self.send("GET", &url, "application/vnd.github.diff", None, None, MAX_DIFF)?.body)
    }

    pub fn default_branch(&self, repo: &RepoId) -> Result<String, ApiError> {
        let value = self.get_json(&format!("/repos/{}", repo.full_name()))?;
        value["default_branch"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| ApiError::Status(200, "no default branch".into()))
    }

    /// Runs `workflow` on `git_ref` with `inputs`.
    pub fn dispatch(
        &self,
        repo: &RepoId,
        workflow: &str,
        git_ref: &str,
        inputs: &BTreeMap<String, String>,
    ) -> Result<(), ApiError> {
        let url = format!("{}/repos/{}/actions/workflows/{}/dispatches", self.rest, repo.full_name(), workflow.trim());
        let body = json!({ "ref": git_ref, "inputs": inputs });
        self.send(
            "POST",
            &url,
            "application/vnd.github+json",
            Some(("application/json", body.to_string())),
            None,
            MAX_BODY,
        )?;
        Ok(())
    }

    /// The latest run of `workflow`, if it ever ran.
    pub fn latest_run(&self, repo: &RepoId, workflow: &str) -> Result<Option<Run>, ApiError> {
        let value = self.get_json(&format!(
            "/repos/{}/actions/workflows/{}/runs?per_page=1",
            repo.full_name(),
            workflow.trim()
        ))?;
        let run = &value["workflow_runs"][0];
        if run.is_null() {
            return Ok(None);
        }
        Ok(Some(Run {
            status: run["status"].as_str().unwrap_or_default().into(),
            conclusion: run["conclusion"].as_str().map(str::to_string),
            url: run["html_url"].as_str().unwrap_or_default().into(),
        }))
    }

    /// Starts the device flow for `client_id`. Only an OAuth app is sent scopes; a GitHub App's
    /// permissions come from the app.
    pub fn device_code(&self, client_id: &str) -> Result<DeviceCode, ApiError> {
        let body = if is_github_app(client_id) {
            form(&[("client_id", client_id)])
        } else {
            form(&[("client_id", client_id), ("scope", SCOPES)])
        };
        let reply = self.send(
            "POST",
            &format!("{}/login/device/code", self.web),
            "application/json",
            Some(("application/x-www-form-urlencoded", body)),
            None,
            MAX_BODY,
        )?;
        let value = parse(&reply.body)?;
        let text = |name: &str| value[name].as_str().map(str::to_string);
        match (text("device_code"), text("user_code")) {
            (Some(device_code), Some(user_code)) => Ok(DeviceCode {
                device_code,
                user_code,
                verification_uri: text("verification_uri").unwrap_or_else(|| format!("{}/login/device", self.web)),
                interval: value["interval"].as_u64().unwrap_or(5).max(1),
                expires_in: value["expires_in"].as_u64().unwrap_or(900),
            }),
            _ => Err(ApiError::Status(reply.status, text("error_description").or(text("error")).unwrap_or_default())),
        }
    }

    /// Asks once whether the user authorized `device_code` yet.
    pub fn device_token(&self, client_id: &str, device_code: &str) -> Result<DevicePoll, ApiError> {
        let body = form(&[
            ("client_id", client_id),
            ("device_code", device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ]);
        let reply = self.send(
            "POST",
            &format!("{}/login/oauth/access_token", self.web),
            "application/json",
            Some(("application/x-www-form-urlencoded", body)),
            None,
            MAX_BODY,
        )?;
        let value = parse(&reply.body)?;
        if value["access_token"].is_string() {
            return Grant::from_response(&value)
                .map(DevicePoll::Token)
                .ok_or_else(|| ApiError::Status(reply.status, "GitHub returned an unexpected token".into()));
        }
        Ok(match value["error"].as_str() {
            Some("authorization_pending") => DevicePoll::Pending,
            Some("slow_down") => DevicePoll::SlowDown,
            Some("expired_token") => DevicePoll::Failed("The code expired; press Connect GitHub again".into()),
            Some("access_denied") => DevicePoll::Failed("Authorization was denied in the browser".into()),
            other => DevicePoll::Failed(
                value["error_description"].as_str().or(other).unwrap_or("Sign-in failed").chars().take(200).collect(),
            ),
        })
    }
}

impl Api {
    /// The installations of GitHub Apps the user's token reaches (a GitHub App user token reaches
    /// its own app's).
    pub fn installations(&self) -> Result<Vec<u64>, ApiError> {
        let mut ids = Vec::new();
        for page in 1..=10 {
            let value = self.get_json(&format!("/user/installations?per_page=100&page={page}"))?;
            let found: Vec<u64> =
                value["installations"].as_array().into_iter().flatten().filter_map(|i| i["id"].as_u64()).collect();
            let more = found.len() == 100;
            ids.extend(found);
            if !more {
                break;
            }
        }
        Ok(ids)
    }

    /// The repositories, as lowercase "owner/name", the user reaches through installation `id`.
    pub fn installation_repositories(&self, id: u64) -> Result<Vec<String>, ApiError> {
        let mut names = Vec::new();
        for page in 1..=50 {
            let value = self.get_json(&format!("/user/installations/{id}/repositories?per_page=100&page={page}"))?;
            let found: Vec<String> = value["repositories"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|repo| repo["full_name"].as_str().map(str::to_ascii_lowercase))
                .collect();
            let more = found.len() == 100;
            names.extend(found);
            if !more {
                break;
            }
        }
        Ok(names)
    }

    /// Which of `repos` no installation the user reaches covers.
    pub fn uncovered(&self, repos: &[RepoId]) -> Result<Vec<RepoId>, ApiError> {
        let mut covered = BTreeSet::new();
        for id in self.installations()? {
            covered.extend(self.installation_repositories(id)?);
        }
        Ok(repos.iter().filter(|repo| !covered.contains(&repo.full_name().to_ascii_lowercase())).cloned().collect())
    }
}

/// What a token request returns: the token and, for an app that kept token expiration on, its
/// lifetime in seconds. A refresh token, if one comes along, is dropped: refreshing needs the app's
/// secret, which ATOMICS never has. Never Debug the token.
#[derive(Clone, PartialEq)]
pub struct Grant {
    pub token: Token,
    pub expires_in: Option<u64>,
}

impl fmt::Debug for Grant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Grant(<hidden>, expires_in: {:?})", self.expires_in)
    }
}

impl Grant {
    fn from_response(value: &Value) -> Option<Grant> {
        Some(Grant {
            token: Token::new(value["access_token"].as_str()?)?,
            expires_in: value["expires_in"].as_u64().filter(|&seconds| seconds > 0),
        })
    }

    /// As the catalog stores it, with the lifetime turned into a time.
    pub fn stored(&self, now: u64) -> AppGrant {
        AppGrant { token: self.token.secret().to_string(), expires: self.expires_in.map(|seconds| now + seconds) }
    }
}

/// A host's API with the stored token. A token past its expiry, or one GitHub answers 401 to, is
/// removed from the catalog, so the modal offers Connect GitHub again; nothing is refreshed.
pub struct Signed {
    host: String,
    store: Store,
    token: String,
    api: Api,
}

impl Signed {
    /// The stored token for `host`; `None` when none is stored or it has expired.
    pub fn open(endpoints: &Endpoints, store: &Store, host: &str) -> Result<Option<Signed>, ApiError> {
        let grant = match store.github_app_grant(host) {
            Ok(Some(grant)) => grant,
            Ok(None) => return Ok(None),
            Err(error) => return Err(ApiError::Transport(error.to_string())),
        };
        if grant.expires.is_some_and(|expires| unix_now() >= expires) {
            let _ = store.remove_github_app_token(host, &grant.token);
            return Ok(None);
        }
        let Some(token) = Token::new(&grant.token) else { return Ok(None) };
        Ok(Some(Signed {
            host: host.to_string(),
            store: store.clone(),
            api: endpoints.api(host, Some(token)),
            token: grant.token,
        }))
    }

    /// A GitHub App user token (`ghu_`): the only kind whose installations say what it reaches.
    pub fn is_app_user_token(&self) -> bool {
        self.token.starts_with("ghu_")
    }

    /// Runs `call`; a 401 removes the token, which signs this host out.
    pub fn call<T>(&mut self, call: impl Fn(&Api) -> Result<T, ApiError>) -> Result<T, ApiError> {
        let result = call(&self.api);
        if matches!(result, Err(ApiError::Unauthorized)) {
            let _ = self.store.remove_github_app_token(&self.host, &self.token);
        }
        result
    }
}

/// The GraphQL query behind the modal: allowed merge methods, and per open pull request its
/// mergeability, review decision, admin bypass and the head commit's checks.
const DETAILS_QUERY: &str = "query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    defaultBranchRef { name }
    mergeCommitAllowed squashMergeAllowed rebaseMergeAllowed
    pullRequests(states: OPEN, first: 100, orderBy: {field: UPDATED_AT, direction: DESC}) {
      nodes {
        number mergeable mergeStateStatus reviewDecision viewerCanMergeAsAdmin
        commits(last: 1) { nodes { commit { statusCheckRollup { state contexts(first: 50) { nodes {
          __typename
          ... on CheckRun { name conclusion status }
          ... on StatusContext { context state }
        } } } } } }
      }
    }
  }
}";

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RepoDetails {
    pub methods: Vec<MergeMethod>,
    pub default_branch: String,
    pub pulls: BTreeMap<u64, Details>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// queued, in_progress or completed.
    pub status: String,
    pub conclusion: Option<String>,
    pub url: String,
}

impl Run {
    pub fn label(&self) -> String {
        match &self.conclusion {
            Some(conclusion) if self.status == "completed" => conclusion.replace('_', " "),
            _ => self.status.replace('_', " "),
        }
    }

    pub fn state(&self) -> CheckState {
        CheckState::from_github(
            self.conclusion.as_deref().filter(|_| self.status == "completed").unwrap_or(&self.status),
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
    pub expires_in: u64,
}

#[derive(Debug, PartialEq)]
pub enum DevicePoll {
    Pending,
    SlowDown,
    Token(Grant),
    Failed(String),
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default()
}

fn parse(body: &str) -> Result<Value, ApiError> {
    serde_json::from_str(body).map_err(|_| ApiError::Status(200, "GitHub's answer was not JSON".into()))
}

/// GitHub's `message`, short; never the request.
fn message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value["message"].as_str().map(|message| message.chars().take(200).collect()))
        .unwrap_or_default()
}

/// A transport error without its URL's query or any header.
fn transport(error: &ureq::Error) -> String {
    match error {
        ureq::Error::Io(error) => error.kind().to_string(),
        ureq::Error::Timeout(_) => "timed out".into(),
        ureq::Error::HostNotFound => "host not found".into(),
        ureq::Error::BodyExceedsLimit(_) => "the answer is too large".into(),
        _ => "connection failed".into(),
    }
}

fn form(fields: &[(&str, &str)]) -> String {
    fields.iter().map(|(name, value)| format!("{name}={}", encode(value))).collect::<Vec<_>>().join("&")
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b' ' => "+".into(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The token the `gh` CLI holds for `host`, if it is installed and signed in. Its output is never
/// logged or shown, and the token is not stored until the user confirms.
pub fn gh_token(program: Option<&Path>, host: &str) -> Option<Token> {
    if !valid_host(host) {
        return None;
    }
    let program = match program {
        Some(program) => program.to_path_buf(),
        None => crate::setup::find_program("gh")?,
    };
    let mut command = Command::new(program);
    command
        .args(["auth", "token", "--hostname", host])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(path) = crate::setup::SHELL_PATH.as_ref() {
        command.env("PATH", path);
    }
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    Token::new(std::str::from_utf8(&output.stdout).ok()?)
}

/// The GitHub repositories of the projects' remotes, then `[github] repos`, each once.
pub fn repositories(projects: &[PathBuf], config: &GithubConfig) -> Vec<RepoId> {
    let mut seen = BTreeSet::new();
    let mut repos = Vec::new();
    for project in projects {
        let listed =
            atomics_core::git::git(project, &["config", "--get-regexp", r"^remote\..*\.url$"]).unwrap_or_default();
        for url in listed.lines().filter_map(|line| line.split_once(' ').map(|(_, url)| url)) {
            if let Some(repo) = RepoId::from_remote(url, config)
                && seen.insert(repo.clone())
            {
                repos.push(repo);
            }
        }
    }
    for repo in config.repos.iter().filter_map(|text| RepoId::parse(text)) {
        if seen.insert(repo.clone()) {
            repos.push(repo);
        }
    }
    repos
}

/// What one poll asks for: the projects whose remotes count, `[github]`, the ETag of each list
/// seen so far, the hosts whose viewer is known, and whether the modal wants details.
#[derive(Debug, Clone, Default)]
pub struct PollRequest {
    pub projects: Vec<PathBuf>,
    pub config: GithubConfig,
    pub etags: BTreeMap<RepoId, String>,
    pub viewers: BTreeSet<String>,
    pub details: bool,
}

#[derive(Debug, Default)]
pub struct Polled {
    pub repos: Vec<RepoId>,
    /// Per host, the viewer's login when it was not known yet.
    pub viewers: BTreeMap<String, Result<String, ApiError>>,
    /// Per repository: `None` when unchanged since its ETag.
    pub lists: Vec<(RepoId, Result<Listed, ApiError>)>,
    pub details: Vec<(RepoId, Result<RepoDetails, ApiError>)>,
    /// Hosts with no usable token stored, including one that expired or that GitHub refused.
    pub signed_out: BTreeSet<String>,
    /// With `[github] app_slug` and a GitHub App user token: per host, the listed repositories the
    /// app is not installed on. Checked only while the modal is open.
    pub uncovered: BTreeMap<String, Vec<RepoId>>,
    /// Details and coverage were asked for, so `uncovered` replaces what was known.
    pub checked: bool,
}

/// One poll of every repository, each host with its own token from the catalog. A host stops at its first 401 or rate limit, so a refused token is not tried again and again.
pub fn poll(endpoints: &Endpoints, store: &Store, request: &PollRequest) -> Polled {
    let mut polled = Polled {
        repos: repositories(&request.projects, &request.config),
        checked: request.details,
        ..Polled::default()
    };
    let hosts: BTreeSet<String> = polled.repos.iter().map(|repo| repo.host.clone()).collect();
    for host in hosts {
        let mut signed = match Signed::open(endpoints, store, &host) {
            Ok(Some(signed)) => signed,
            Ok(None) | Err(ApiError::Unauthorized) => {
                polled.signed_out.insert(host);
                continue;
            }
            Err(error) => {
                polled.viewers.insert(host, Err(error));
                continue;
            }
        };
        if !request.viewers.contains(&host) {
            let viewer = signed.call(Api::viewer);
            let stop = matches!(viewer, Err(ApiError::Unauthorized | ApiError::RateLimited { .. }));
            polled.viewers.insert(host.clone(), viewer);
            if stop {
                continue;
            }
        }
        let repos: Vec<RepoId> = polled.repos.iter().filter(|repo| repo.host == host).cloned().collect();
        for repo in &repos {
            let list = signed.call(|api| api.pulls(repo, request.etags.get(repo).map(String::as_str)));
            let stop = matches!(list, Err(ApiError::Unauthorized | ApiError::RateLimited { .. }));
            polled.lists.push((repo.clone(), list));
            if stop {
                break;
            }
            if request.details {
                polled.details.push((repo.clone(), signed.call(|api| api.details(repo))));
            }
        }
        // Coverage is a hint: a failed check shows no install rows rather than an error.
        if request.details
            && signed.is_app_user_token()
            && let Ok(uncovered) = signed.call(|api| api.uncovered(&repos))
        {
            polled.uncovered.insert(host, uncovered);
        }
    }
    polled
}

#[cfg(test)]
pub(crate) mod fake;

#[cfg(test)]
mod tests {
    use super::*;
    use fake::FakeGithub;

    const TOKEN: &str = "gho_PRIVATE-github-token";

    fn repo() -> RepoId {
        RepoId::parse("octo/widgets").unwrap()
    }

    #[test]
    fn github_token_never_prints_and_rejects_strangers() {
        let token = Token::new(TOKEN).unwrap();
        assert_eq!(format!("{token:?}"), "Token(<hidden>)");
        assert!(Token::new("sk-ant-oat01-misfiled").is_none());
        assert!(Token::new("ghp_has space").is_none());
        assert_eq!(Token::new(" ghp_trimmed\n").unwrap().secret(), "ghp_trimmed");
    }

    #[test]
    fn github_lists_pulls_with_etags_and_reads_details() {
        let fake = FakeGithub::start(TOKEN);
        fake.add_pull(&repo(), fake::pull(7, "alice", &["bob"], "CLEAN", false));
        fake.add_pull(&repo(), fake::pull(8, "bob", &[], "BLOCKED", true));
        let api = Api::at(&fake.url, Token::new(TOKEN));
        assert_eq!(api.viewer().unwrap(), "alice");
        let (pulls, etag) = api.pulls(&repo(), None).unwrap().unwrap();
        assert_eq!(pulls.iter().map(|p| p.number).collect::<Vec<_>>(), [7, 8]);
        assert_eq!(pulls[0].requested, ["bob"]);
        assert_eq!(pulls[0].head_sha, "sha-7");
        assert!(api.pulls(&repo(), etag.as_deref()).unwrap().is_none(), "unchanged lists answer 304");
        assert_eq!(fake.header_of_last("if-none-match").as_deref(), etag.as_deref());
        let details = api.details(&repo()).unwrap();
        assert_eq!(details.methods, [MergeMethod::Squash, MergeMethod::Merge]);
        assert_eq!(details.default_branch, "main");
        assert_eq!(details.pulls[&7].merge_state, "CLEAN");
        assert_eq!(details.pulls[&7].checks, CheckState::Passing);
        assert_eq!(details.pulls[&7].check_runs[0].name, "build");
        assert!(!details.pulls[&7].admin && details.pulls[&8].admin);
        assert_eq!(details.pulls[&8].checks, CheckState::Failing);
        assert_eq!(fake.header_of_last("authorization").as_deref(), Some(format!("Bearer {TOKEN}").as_str()));
        assert_eq!(Api::at(&fake.url, Token::new("ghp_wrong")).viewer().unwrap_err(), ApiError::Unauthorized);
    }

    #[test]
    fn github_rate_limits_become_backoff() {
        let fake = FakeGithub::start(TOKEN);
        fake.rate_limit(Some(42));
        let api = Api::at(&fake.url, Token::new(TOKEN));
        let error = api.pulls(&repo(), None).unwrap_err();
        assert_eq!(error.backoff(None), Some(42));
        fake.rate_limit(None);
        let error = api.pulls(&repo(), None).unwrap_err();
        assert!(matches!(error, ApiError::RateLimited { status: 429, .. }));
        assert_eq!(error.backoff(Some(120)), Some(240));
    }

    #[test]
    fn github_device_flow_polls_until_authorized() {
        let fake = FakeGithub::start(TOKEN);
        let api = Api::at(&fake.url, None);
        let code = api.device_code("client-123").unwrap();
        assert_eq!(code.user_code, "WDJB-MJHT");
        let seen = fake.last_body();
        assert!(seen.contains("client_id=client-123") && seen.contains("scope=repo+workflow"), "{seen}");
        assert_eq!(api.device_token("client-123", &code.device_code).unwrap(), DevicePoll::Pending);
        fake.authorize_device();
        let DevicePoll::Token(grant) = api.device_token("client-123", &code.device_code).unwrap() else {
            panic!("no token")
        };
        assert_eq!(grant.token.secret(), TOKEN);
        assert!(grant.expires_in.is_none(), "an OAuth app's token does not expire");
        assert!(!format!("{grant:?}").contains("PRIVATE"));
    }

    #[test]
    fn github_app_device_flow_sends_no_scope_and_keeps_no_refresh_token() {
        let fake = FakeGithub::start(TOKEN);
        fake.github_app();
        let api = Api::at(&fake.url, None);
        api.device_code("Iv23liAtomicsApp").unwrap();
        let seen = fake.last_body();
        assert!(seen.contains("client_id=Iv23liAtomicsApp") && !seen.contains("scope"), "{seen}");
        fake.authorize_device();
        let DevicePoll::Token(grant) = api.device_token("Iv23liAtomicsApp", "device-123").unwrap() else {
            panic!("no token")
        };
        assert!(grant.token.secret().starts_with("ghu_"));
        // An app that kept expiration on: the expiry is kept, the refresh token is not.
        assert_eq!(grant.expires_in, Some(28800));
        assert_eq!(grant.stored(1_000).expires, Some(29_800));
        assert!(!format!("{grant:?}").contains("PRIVATE"));
        assert!(fake.seen().iter().all(|seen| !seen.body.contains("refresh_token")), "nothing is ever refreshed");
    }

    #[test]
    fn github_expired_or_refused_tokens_sign_out() {
        let (dir, _, store) = crate::credentials::tests::machine(cfg!(target_os = "macos"));
        let fake = FakeGithub::start(TOKEN);
        let endpoints = Endpoints { base: Some(fake.url.clone()), gh: None };
        // Past its expiry: never sent, and removed.
        let expired = AppGrant { token: "ghu_PRIVATE-expired".into(), expires: Some(unix_now() - 1) };
        store.put_github_app_grant("github.com", &expired).unwrap();
        assert!(Signed::open(&endpoints, &store, "github.com").unwrap().is_none());
        assert!(store.github_app_grant("github.com").unwrap().is_none());
        assert!(fake.seen().is_empty());
        // Still valid: used as it is.
        let user = fake.issue_user_token();
        store
            .put_github_app_grant("github.com", &AppGrant { token: user.clone(), expires: Some(unix_now() + 3600) })
            .unwrap();
        let mut signed = Signed::open(&endpoints, &store, "github.com").unwrap().unwrap();
        assert!(signed.is_app_user_token());
        assert_eq!(signed.call(Api::viewer).unwrap(), "alice");
        // A 401 removes it, and nothing tries to refresh it.
        fake.expire_tokens();
        assert_eq!(signed.call(Api::viewer).unwrap_err(), ApiError::Unauthorized);
        assert!(store.github_app_grant("github.com").unwrap().is_none());
        assert!(Signed::open(&endpoints, &store, "github.com").unwrap().is_none());
        assert!(fake.seen().iter().all(|seen| !seen.path.starts_with("/login")));
        // A poll reports the host signed out.
        store.put_github_app("github.com", TOKEN).unwrap();
        let request = PollRequest {
            config: GithubConfig { repos: vec!["octo/widgets".into()], ..GithubConfig::default() },
            ..PollRequest::default()
        };
        let polled = poll(&endpoints, &store, &request);
        assert_eq!(polled.viewers["github.com"], Err(ApiError::Unauthorized));
        assert!(store.github_app_grant("github.com").unwrap().is_none());
        assert!(poll(&endpoints, &store, &request).signed_out.contains("github.com"));
        let argv = std::fs::read_to_string(dir.path().join("keychain-argv")).unwrap();
        assert!(!argv.contains("PRIVATE"));
    }

    #[test]
    fn github_app_coverage_finds_repositories_without_the_app() {
        let (_dir, _, store) = crate::credentials::tests::machine(cfg!(target_os = "macos"));
        let fake = FakeGithub::start(TOKEN);
        fake.install("octo", &["octo/widgets"]);
        fake.install("me", &["me/tools", "Me/Notes"]);
        let user = fake.issue_user_token();
        store.put_github_app_grant("github.com", &AppGrant { token: user.clone(), expires: None }).unwrap();
        let repos: Vec<RepoId> = ["octo/widgets", "octo/gears", "me/notes", "other/thing"]
            .iter()
            .map(|r| RepoId::parse(r).unwrap())
            .collect();
        let api = Api::at(&fake.url, Token::new(&user));
        let uncovered: Vec<String> = api.uncovered(&repos).unwrap().iter().map(ToString::to_string).collect();
        assert_eq!(uncovered, ["octo/gears", "other/thing"]);
        // The poll checks coverage only with a GitHub App user token and the modal open.
        let endpoints = Endpoints { base: Some(fake.url.clone()), gh: None };
        let mut request = PollRequest {
            config: GithubConfig { repos: vec!["octo/widgets".into(), "octo/gears".into()], ..GithubConfig::default() },
            details: true,
            ..PollRequest::default()
        };
        let polled = poll(&endpoints, &store, &request);
        assert_eq!(polled.uncovered["github.com"], [RepoId::parse("octo/gears").unwrap()]);
        request.details = false;
        assert!(poll(&endpoints, &store, &request).uncovered.is_empty(), "only while the modal is open");
        request.details = true;
        store.put_github_app("github.com", TOKEN).unwrap();
        assert!(
            poll(&endpoints, &store, &request).uncovered.is_empty(),
            "an OAuth token reaches what the user reaches"
        );
    }

    #[test]
    fn github_repositories_come_from_remotes_and_config() {
        let dir = crate::test_support::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let git = |args: &[&str]| atomics_core::git::git(&project, args).unwrap();
        git(&["init", "-q"]);
        git(&["remote", "add", "origin", "git@github.com:octo/widgets.git"]);
        git(&["remote", "add", "fork", "https://git.example.com/me/widgets"]);
        git(&["remote", "add", "other", "https://gitlab.com/octo/widgets.git"]);
        let mut config =
            GithubConfig { repos: vec!["octo/widgets".into(), "octo/gears".into()], ..GithubConfig::default() };
        let names = |config: &GithubConfig| {
            repositories(std::slice::from_ref(&project), config).iter().map(ToString::to_string).collect::<Vec<_>>()
        };
        assert_eq!(names(&config), ["octo/widgets", "octo/gears"]);
        config.hosts = vec!["git.example.com".into()];
        assert_eq!(names(&config), ["octo/widgets", "git.example.com/me/widgets", "octo/gears"]);
    }

    #[test]
    fn github_gh_token_is_read_without_storing_it() {
        let dir = crate::test_support::tempdir().unwrap();
        let gh = fake::gh(dir.path(), Some(TOKEN));
        assert_eq!(gh_token(Some(&gh), "github.com"), Token::new(TOKEN));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("gh-argv")).unwrap().trim(),
            "auth token --hostname github.com"
        );
        let missing = fake::gh(dir.path(), None);
        assert!(gh_token(Some(&missing), "github.com").is_none());
        assert!(gh_token(Some(&gh), "not a host").is_none());
    }
}
