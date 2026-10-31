//! An in-process stand-in for GitHub: the REST endpoints and the GraphQL query the integration
//! uses, and the OAuth device flow. It checks the bearer token, answers 304 to a matching ETag,
//! merges only what GitHub would (admins past BLOCKED), and records every request.
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use atomics_core::github::RepoId;
use serde_json::{Value, json};

#[derive(Debug, Clone, Default)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.clone())
    }
}

#[derive(Debug, Clone)]
pub struct FakePull {
    pub number: u64,
    pub base: String,
    pub author: String,
    pub requested: Vec<String>,
    pub merge_state: String,
    pub admin: bool,
    pub draft: bool,
    /// The head commit; `sha-<number>` until a test pushes.
    pub head_sha: String,
    /// The head commit's check rollup: SUCCESS, FAILURE or PENDING.
    pub checks: String,
    pub diff: String,
}

/// A pull request whose checks pass when `merge_state` is CLEAN and fail otherwise.
pub fn pull(number: u64, author: &str, requested: &[&str], merge_state: &str, admin: bool) -> FakePull {
    FakePull {
        number,
        base: "main".into(),
        author: author.into(),
        requested: requested.iter().map(|login| login.to_string()).collect(),
        merge_state: merge_state.into(),
        admin,
        draft: false,
        head_sha: format!("sha-{number}"),
        checks: if merge_state == "CLEAN" { "SUCCESS" } else { "FAILURE" }.into(),
        diff: format!(
            "diff --git a/src/pr{number}.rs b/src/pr{number}.rs\nnew file mode 100644\nindex 0000000..1111111\n--- /dev/null\n+++ b/src/pr{number}.rs\n@@ -0,0 +1,2 @@\n+// pull request {number}\n+fn changed() {{}}\ndiff --git a/README.md b/README.md\nindex 2222222..3333333 100644\n--- a/README.md\n+++ b/README.md\n@@ -1,2 +1,2 @@\n # Widgets\n-old line\n+new line from {number}\n"
        ),
    }
}

#[derive(Default)]
struct Data {
    token: String,
    /// Tokens the API accepts: `token` and those the device flow or a test issued.
    valid: Vec<String>,
    /// GitHub App mode with token expiration on: issued tokens come with `expires_in` and a
    /// refresh token, which ATOMICS must ignore.
    expiring: bool,
    issued: u64,
    /// Installations of the app: an id, its account and the repositories it covers.
    installations: Vec<(u64, String, Vec<String>)>,
    viewer: String,
    repos: BTreeMap<String, (u64, Vec<FakePull>)>,
    branch_rules: BTreeMap<(String, String), (u16, Value)>,
    rulesets: BTreeMap<(String, u64), (u16, Value)>,
    permissions: BTreeMap<String, String>,
    seen: Vec<Seen>,
    rate_limit: Option<Option<u64>>,
    device_authorized: bool,
    merges: Vec<(String, u64, Value)>,
    dispatches: Vec<(String, String, Value)>,
    runs: BTreeMap<(String, String), Value>,
}

pub struct FakeGithub {
    pub url: String,
    data: Arc<Mutex<Data>>,
}

impl FakeGithub {
    /// Accepts `token` (and the device flow's token, which is the same) for the viewer "alice".
    pub fn start(token: &str) -> FakeGithub {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let data = Arc::new(Mutex::new(Data {
            token: token.into(),
            valid: vec![token.into()],
            viewer: "alice".into(),
            ..Data::default()
        }));
        let shared = data.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let data = shared.clone();
                std::thread::spawn(move || serve(stream, &data));
            }
        });
        FakeGithub { url, data }
    }

    pub fn add_pull(&self, repo: &RepoId, pull: FakePull) {
        let mut data = self.data.lock().unwrap();
        let entry = data.repos.entry(repo.full_name()).or_default();
        entry.0 += 1;
        entry.1.push(pull);
    }

    pub fn branch_rules(&self, repo: &RepoId, base: &str, status: u16, rules: Value) {
        self.data.lock().unwrap().branch_rules.insert((repo.full_name(), base.into()), (status, rules));
    }

    pub fn ruleset(&self, repo: &RepoId, id: u64, status: u16, ruleset: Value) {
        self.data.lock().unwrap().rulesets.insert((repo.full_name(), id), (status, ruleset));
    }

    pub fn viewer_permission(&self, repo: &RepoId, permission: &str) {
        self.data.lock().unwrap().permissions.insert(repo.full_name(), permission.into());
    }

    /// A push to pull request `number`: its head becomes `sha`, and the list's ETag changes.
    pub fn push(&self, repo: &RepoId, number: u64, sha: &str) {
        let mut data = self.data.lock().unwrap();
        let (version, pulls) = data.repos.get_mut(&repo.full_name()).expect("a pull request of this repository");
        pulls.iter_mut().find(|pull| pull.number == number).expect("the pull request").head_sha = sha.into();
        *version += 1;
    }

    /// Answers 429 to everything, with `retry-after` when given.
    pub fn rate_limit(&self, retry_after: Option<u64>) {
        self.data.lock().unwrap().rate_limit = Some(retry_after);
    }

    pub fn authorize_device(&self) {
        self.data.lock().unwrap().device_authorized = true;
    }

    /// GitHub App mode with token expiration on: the device flow issues `ghu_` tokens that expire
    /// after 8 hours, with a refresh token beside them.
    pub fn github_app(&self) {
        self.data.lock().unwrap().expiring = true;
    }

    /// A GitHub App user token (`ghu_`) the API accepts, for a test that stores it itself.
    pub fn issue_user_token(&self) -> String {
        let mut data = self.data.lock().unwrap();
        data.issued += 1;
        let token = format!("ghu_PRIVATE-user-{}", data.issued);
        data.valid.push(token.clone());
        token
    }

    /// Every token stops working, as one past its expiry or revoked does.
    pub fn expire_tokens(&self) {
        self.data.lock().unwrap().valid.clear();
    }

    /// Installs the app on `account`, covering `repos` ("owner/name").
    pub fn install(&self, account: &str, repos: &[&str]) {
        let mut data = self.data.lock().unwrap();
        let id = 100 + data.installations.len() as u64;
        data.installations.push((id, account.into(), repos.iter().map(|repo| repo.to_string()).collect()));
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.data.lock().unwrap().seen.clone()
    }

    pub fn header_of_last(&self, name: &str) -> Option<String> {
        self.data.lock().unwrap().seen.last().and_then(|seen| seen.header(name))
    }

    pub fn last_body(&self) -> String {
        self.data.lock().unwrap().seen.last().map(|seen| seen.body.clone()).unwrap_or_default()
    }

    pub fn merges(&self) -> Vec<(String, u64, Value)> {
        self.data.lock().unwrap().merges.clone()
    }

    pub fn dispatches(&self) -> Vec<(String, String, Value)> {
        self.data.lock().unwrap().dispatches.clone()
    }
}

fn serve(stream: TcpStream, data: &Mutex<Data>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or_default().to_string(), parts.next().unwrap_or_default().to_string());
    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() || header == "\r\n" || header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.trim_end().split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let length =
        headers.iter().find(|(name, _)| name == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0; length];
    let _ = reader.read_exact(&mut body);
    let seen = Seen { method, path: target, headers, body: String::from_utf8_lossy(&body).into_owned() };
    let (status, extra, body) = respond(&seen, &mut data.lock().unwrap());
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        304 => "Not Modified",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        429 => "Too Many Requests",
        _ => "Status",
    };
    let mut out = format!("HTTP/1.1 {status} {reason}\r\nconnection: close\r\ncontent-length: {}\r\n", body.len());
    for (name, value) in extra {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("\r\n");
    out.push_str(&body);
    let mut stream = reader.into_inner();
    let _ = stream.write_all(out.as_bytes());
}

type Response = (u16, Vec<(&'static str, String)>, String);

/// A token answer: the plain token, or in GitHub App mode a new expiring user token with a refresh token.
fn issue(data: &mut Data) -> Value {
    if !data.expiring {
        return json!({ "access_token": data.token, "token_type": "bearer", "scope": "repo,workflow" });
    }
    data.issued += 1;
    let token = format!("ghu_PRIVATE-user-{}", data.issued);
    data.valid.push(token.clone());
    json!({ "access_token": token, "token_type": "bearer", "scope": "", "expires_in": 28800,
            "refresh_token": format!("ghr_PRIVATE-refresh-{}", data.issued), "refresh_token_expires_in": 15897600 })
}

fn json_reply(status: u16, value: Value) -> Response {
    (status, vec![("content-type", "application/json".into())], value.to_string())
}

fn respond(seen: &Seen, data: &mut Data) -> Response {
    data.seen.push(seen.clone());
    let path = seen.path.split('?').next().unwrap_or_default().to_string();
    // The device flow needs no token.
    match (seen.method.as_str(), path.as_str()) {
        ("POST", "/login/device/code") => {
            return json_reply(
                200,
                json!({ "device_code": "device-123", "user_code": "WDJB-MJHT",
                        "verification_uri": "https://github.com/login/device", "expires_in": 900, "interval": 1 }),
            );
        }
        ("POST", "/login/oauth/access_token") => {
            return if data.device_authorized {
                let answer = issue(data);
                json_reply(200, answer)
            } else {
                json_reply(200, json!({ "error": "authorization_pending" }))
            };
        }
        _ => {}
    }
    if let Some(retry_after) = data.rate_limit {
        let mut headers = vec![("x-ratelimit-remaining", "0".to_string())];
        headers.extend(retry_after.map(|seconds| ("retry-after", seconds.to_string())));
        return (429, headers, json!({ "message": "API rate limit exceeded" }).to_string());
    }
    let bearer = seen.header("authorization").and_then(|value| value.strip_prefix("Bearer ").map(str::to_string));
    if !bearer.is_some_and(|token| data.valid.contains(&token)) {
        return json_reply(401, json!({ "message": "Bad credentials" }));
    }
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (seen.method.as_str(), segments.as_slice()) {
        ("GET", ["user"]) => json_reply(200, json!({ "login": data.viewer })),
        ("GET", ["user", "installations"]) => {
            let installations: Vec<Value> = data
                .installations
                .iter()
                .map(|(id, account, _)| json!({ "id": id, "account": { "login": account } }))
                .collect();
            json_reply(200, json!({ "total_count": installations.len(), "installations": installations }))
        }
        ("GET", ["user", "installations", id, "repositories"]) => {
            match data.installations.iter().find(|(own, _, _)| own.to_string() == *id) {
                Some((_, _, repos)) => {
                    let repositories: Vec<Value> = repos.iter().map(|name| json!({ "full_name": name })).collect();
                    json_reply(200, json!({ "total_count": repositories.len(), "repositories": repositories }))
                }
                None => json_reply(404, json!({ "message": "Not Found" })),
            }
        }
        ("POST", ["graphql"]) => graphql(seen, data),
        ("GET", ["repos", owner, name, "rules", "branches", base]) => {
            let (status, value) = data
                .branch_rules
                .get(&(format!("{owner}/{name}"), base.to_string()))
                .cloned()
                .unwrap_or((200, json!([])));
            if status != 200 {
                return json_reply(status, value);
            }
            let page = seen
                .path
                .split('?')
                .nth(1)
                .and_then(|query| {
                    query
                        .split('&')
                        .find_map(|part| part.strip_prefix("page=").and_then(|page| page.parse::<usize>().ok()))
                })
                .unwrap_or(1);
            let rules: Vec<Value> =
                value.as_array().into_iter().flatten().skip(page.saturating_sub(1) * 100).take(100).cloned().collect();
            json_reply(200, json!(rules))
        }
        ("GET", ["repos", owner, name, "rulesets", id]) => {
            let answer =
                id.parse::<u64>().ok().and_then(|id| data.rulesets.get(&(format!("{owner}/{name}"), id))).cloned();
            let (status, value) = answer.unwrap_or((404, json!({ "message": "Not Found" })));
            json_reply(status, value)
        }
        ("GET", ["repos", owner, name]) if data.repos.contains_key(&format!("{owner}/{name}")) => {
            json_reply(200, json!({ "default_branch": "main" }))
        }
        ("GET", ["repos", owner, name, "pulls"]) => {
            let (version, pulls) = data.repos.get(&format!("{owner}/{name}")).cloned().unwrap_or_default();
            let etag = format!("\"v{version}\"");
            if seen.header("if-none-match").as_deref() == Some(etag.as_str()) {
                return (304, vec![("etag", etag)], String::new());
            }
            let list: Vec<Value> = pulls
                .iter()
                .map(|pull| {
                    json!({
                        "number": pull.number,
                        "title": format!("Pull request {}", pull.number),
                        "html_url": format!("https://github.com/{owner}/{name}/pull/{}", pull.number),
                        "user": { "login": pull.author },
                        "draft": pull.draft,
                        "head": { "ref": format!("feature-{}", pull.number), "sha": pull.head_sha },
                        "base": { "ref": pull.base },
                        "requested_reviewers": pull.requested.iter().map(|login| json!({ "login": login })).collect::<Vec<_>>(),
                    })
                })
                .collect();
            (200, vec![("etag", etag), ("content-type", "application/json".into())], Value::from(list).to_string())
        }
        ("GET", ["repos", owner, name, "pulls", number]) => {
            let pull = data
                .repos
                .get(&format!("{owner}/{name}"))
                .and_then(|(_, pulls)| pulls.iter().find(|pull| pull.number.to_string() == *number));
            match pull {
                Some(pull) if seen.header("accept").as_deref() == Some("application/vnd.github.diff") => {
                    (200, vec![("content-type", "text/plain".into())], pull.diff.clone())
                }
                Some(pull) => json_reply(
                    200,
                    json!({ "number": pull.number,
                            "head": { "ref": format!("feature-{}", pull.number), "sha": pull.head_sha },
                            "base": { "ref": pull.base } }),
                ),
                None => json_reply(404, json!({ "message": "Not Found" })),
            }
        }
        ("PUT", ["repos", owner, name, "pulls", number, "merge"]) => {
            let repo = format!("{owner}/{name}");
            let request: Value = serde_json::from_str(&seen.body).unwrap_or_default();
            let Some((version, pulls)) = data.repos.get_mut(&repo) else {
                return json_reply(404, json!({ "message": "Not Found" }));
            };
            let Some(index) = pulls.iter().position(|pull| pull.number.to_string() == *number) else {
                return json_reply(404, json!({ "message": "Not Found" }));
            };
            let pull = &pulls[index];
            if request["sha"].as_str() != Some(pull.head_sha.as_str()) {
                return json_reply(
                    409,
                    json!({ "message": "Head branch was modified. Review and try the merge again." }),
                );
            }
            let allowed = matches!(pull.merge_state.as_str(), "CLEAN" | "HAS_HOOKS" | "UNSTABLE")
                || (pull.admin && matches!(pull.merge_state.as_str(), "BLOCKED" | "BEHIND"));
            if !allowed {
                return json_reply(405, json!({ "message": "Required status check \"build\" is failing." }));
            }
            let number = pull.number;
            pulls.remove(index);
            *version += 1;
            data.merges.push((repo, number, request));
            json_reply(200, json!({ "merged": true, "message": "Pull Request successfully merged", "sha": "merged" }))
        }
        ("POST", ["repos", owner, name, "actions", "workflows", workflow, "dispatches"]) => {
            let repo = format!("{owner}/{name}");
            let request: Value = serde_json::from_str(&seen.body).unwrap_or_default();
            data.runs.insert(
                (repo.clone(), workflow.to_string()),
                json!({ "status": "queued", "conclusion": null,
                        "html_url": format!("https://github.com/{repo}/actions/runs/1") }),
            );
            data.dispatches.push((repo, workflow.to_string(), request));
            (204, vec![], String::new())
        }
        ("GET", ["repos", owner, name, "actions", "workflows", workflow, "runs"]) => {
            let run = data.runs.get(&(format!("{owner}/{name}"), workflow.to_string())).cloned();
            json_reply(
                200,
                json!({ "total_count": run.is_some() as u8, "workflow_runs": run.into_iter().collect::<Vec<_>>() }),
            )
        }
        _ => json_reply(404, json!({ "message": "Not Found" })),
    }
}

fn graphql(seen: &Seen, data: &Data) -> Response {
    let request: Value = serde_json::from_str(&seen.body).unwrap_or_default();
    let variables = &request["variables"];
    let repo = format!(
        "{}/{}",
        variables["owner"].as_str().unwrap_or_default(),
        variables["name"].as_str().unwrap_or_default()
    );
    let Some((_, pulls)) = data.repos.get(&repo) else {
        return json_reply(200, json!({ "data": { "repository": null } }));
    };
    let nodes: Vec<Value> = pulls
        .iter()
        .map(|pull| {
            json!({
                "number": pull.number,
                "baseRefName": pull.base,
                "mergeable": if pull.merge_state == "DIRTY" { "CONFLICTING" } else { "MERGEABLE" },
                "mergeStateStatus": pull.merge_state,
                "reviewDecision": if pull.merge_state == "CLEAN" { "APPROVED" } else { "REVIEW_REQUIRED" },
                "viewerCanMergeAsAdmin": pull.admin,
                "commits": { "nodes": [{ "commit": { "statusCheckRollup": {
                    "state": pull.checks,
                    "contexts": { "nodes": [
                        { "__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": pull.checks },
                        { "__typename": "StatusContext", "context": "lint", "state": "SUCCESS" },
                    ] },
                } } }] },
            })
        })
        .collect();
    json_reply(
        200,
        json!({ "data": { "repository": {
            "defaultBranchRef": { "name": "main" },
            "viewerPermission": data.permissions.get(&repo).map(String::as_str).unwrap_or("WRITE"),
            "mergeCommitAllowed": true, "squashMergeAllowed": true, "rebaseMergeAllowed": false,
            "pullRequests": { "nodes": nodes },
        } } }),
    )
}

/// A `gh` that records its arguments in `gh-argv` beside it and prints `token`, or fails like a
/// signed-out gh when there is none.
pub fn gh(dir: &Path, token: Option<&str>) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let program = dir.join(if token.is_some() { "gh" } else { "gh-signed-out" });
    let script = match token {
        Some(token) => format!("#!/bin/sh\nprintf '%s\\n' \"$*\" > \"${{0%/*}}/gh-argv\"\nprintf '%s\\n' '{token}'\n"),
        None => "#!/bin/sh\necho 'You are not logged into any GitHub hosts.' >&2\nexit 1\n".to_string(),
    };
    std::fs::write(&program, script).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    program
}
