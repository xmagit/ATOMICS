//! The UI-free half of the GitHub integration: `[github]` in the config, which git remotes are
//! GitHub repositories, workflow placeholders, the counter's filter, which merge buttons a pull
//! request gets, and how long to wait after a rate limit. Nothing here names a repository.
use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

pub const DEFAULT_HOST: &str = "github.com";

/// The public client ID of the ATOMICS GitHub App, which Connect GitHub's device flow signs in with.
/// It is no secret; `[github] client_id` replaces it for GitHub Enterprise or a fork.
pub const DEFAULT_CLIENT_ID: &str = "Iv23liEnPEe65mKmXgWf";

/// The ATOMICS GitHub App's slug, for "Install ATOMICS on <owner>"; `[github] app_slug` replaces it.
pub const DEFAULT_APP_SLUG: &str = "atomiskide";

/// What the device flow asks an OAuth app for: pull requests and merges (`repo`), and workflows.
/// A GitHub App gets no scopes: its permissions come from the app.
pub const SCOPES: &str = "repo workflow";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GithubConfig {
    /// The top-bar counter and the controls modal; off hides both.
    pub enabled: bool,
    /// GitHub Enterprise Server hosts whose remotes count, besides github.com.
    pub hosts: Vec<String>,
    /// Repositories beyond the loaded projects' remotes: "owner/name" or "host/owner/name".
    pub repos: Vec<String>,
    /// The OAuth app or GitHub App the device flow signs in with; empty is ATOMICS's own app.
    pub client_id: String,
    /// The GitHub App's slug; set, ATOMICS offers to install it where a listed repository lacks it.
    pub app_slug: String,
    /// Which open pull requests the counter counts.
    pub filter: Filter,
    pub poll_seconds: u64,
    pub actions: Vec<Action>,
}

impl Default for GithubConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            hosts: Vec::new(),
            repos: Vec::new(),
            client_id: String::new(),
            app_slug: String::new(),
            filter: Filter::Mine,
            poll_seconds: 60,
            actions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Filter {
    /// Pull requests the signed-in user opened.
    #[default]
    Mine,
    /// Pull requests waiting for the signed-in user's review.
    Review,
    All,
}

impl Filter {
    pub fn matches(self, pull: &Pull, viewer: &str) -> bool {
        match self {
            Filter::Mine => pull.author.eq_ignore_ascii_case(viewer),
            Filter::Review => pull.requested.iter().any(|login| login.eq_ignore_ascii_case(viewer)),
            Filter::All => true,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Filter::Mine => "yours",
            Filter::Review => "waiting for your review",
            Filter::All => "open",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// A button on each pull request; every placeholder is available.
    #[default]
    Pr,
    /// A button on the repository; only `{repo}` is available.
    Repo,
}

/// A `[[github.actions]]` entry: a workflow the modal can dispatch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Action {
    pub label: String,
    /// "owner/name" or "host/owner/name"; empty offers the action on every listed repository.
    pub repo: String,
    /// A workflow file name such as `release.yml`, or its numeric id.
    pub workflow: String,
    /// The ref the workflow runs on; empty is `{base}` for PR actions, the default branch otherwise.
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub scope: Scope,
    /// Workflow inputs; an empty value is asked for when the action is confirmed.
    pub inputs: BTreeMap<String, String>,
}

impl Action {
    /// The ref template, with the scope's default.
    pub fn ref_template(&self) -> Option<&str> {
        match (self.git_ref.trim(), self.scope) {
            ("", Scope::Pr) => Some("{base}"),
            ("", Scope::Repo) => None,
            (text, _) => Some(text),
        }
    }

    /// Whether this action is offered on `repo`.
    pub fn applies_to(&self, repo: &RepoId) -> bool {
        self.repo.trim().is_empty() || RepoId::parse(&self.repo).is_some_and(|own| own == *repo)
    }
}

impl GithubConfig {
    /// What is wrong with `[github]`, one line per problem; values that cannot work are reported,
    /// never silently dropped.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for host in &self.hosts {
            if !valid_host(host) {
                problems.push(format!("[github] hosts: {host:?} is not a host name"));
            }
        }
        if !valid_name(self.app_slug()) || self.app_slug().contains('.') {
            problems.push(format!("[github] app_slug: {:?} is not a GitHub App slug", self.app_slug));
        }
        if self.client_id().is_empty() || !self.client_id().bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.') {
            problems.push("[github] client_id: not a client ID".into());
        }
        for repo in &self.repos {
            if RepoId::parse(repo).is_none() {
                problems.push(format!("[github] repos: {repo:?} is not \"owner/name\" or \"host/owner/name\""));
            }
        }
        for (i, action) in self.actions.iter().enumerate() {
            let name = if action.label.trim().is_empty() { format!("#{}", i + 1) } else { action.label.clone() };
            if action.label.trim().is_empty() {
                problems.push(format!("[[github.actions]] {name}: label is empty"));
            }
            if !valid_workflow(&action.workflow) {
                problems.push(format!("[[github.actions]] {name}: workflow must be a file name or an id"));
            }
            if !action.repo.trim().is_empty() && RepoId::parse(&action.repo).is_none() {
                problems.push(format!("[[github.actions]] {name}: repo {:?} is not \"owner/name\"", action.repo));
            }
            let sample = PrContext { number: 1, head_sha: "0", head: "head", base: "base" };
            let pr = (action.scope == Scope::Pr).then_some(&sample);
            let templates = action.ref_template().into_iter().chain(action.inputs.values().map(String::as_str));
            for template in templates {
                if let Err(error) = fill(template, "owner/name", pr) {
                    problems.push(format!("[[github.actions]] {name}: {error}"));
                }
            }
        }
        problems
    }

    /// The client ID Connect GitHub's device flow uses: `[github] client_id`, else the ATOMICS app's.
    pub fn client_id(&self) -> &str {
        let id = self.client_id.trim();
        if id.is_empty() { DEFAULT_CLIENT_ID } else { id }
    }

    /// The GitHub App whose installations cover the listed repositories: `[github] app_slug`, else
    /// the ATOMICS app's.
    pub fn app_slug(&self) -> &str {
        let slug = self.app_slug.trim();
        if slug.is_empty() { DEFAULT_APP_SLUG } else { slug }
    }

    /// Seconds between polls, never below 15.
    pub fn poll_interval(&self) -> u64 {
        self.poll_seconds.max(15)
    }

    /// Whether remotes on `host` count.
    pub fn knows_host(&self, host: &str) -> bool {
        host.eq_ignore_ascii_case(DEFAULT_HOST) || self.hosts.iter().any(|known| known.eq_ignore_ascii_case(host))
    }
}

/// A GitHub repository on a host.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoId {
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl RepoId {
    /// "owner/name" on github.com, or "host/owner/name".
    pub fn parse(text: &str) -> Option<RepoId> {
        let parts: Vec<&str> = text.trim().split('/').collect();
        let (host, owner, name) = match parts.as_slice() {
            [owner, name] => (DEFAULT_HOST, *owner, *name),
            [host, owner, name] => (*host, *owner, *name),
            _ => return None,
        };
        RepoId::new(host, owner, name)
    }

    fn new(host: &str, owner: &str, name: &str) -> Option<RepoId> {
        let name = name.strip_suffix(".git").unwrap_or(name);
        let host = host.to_ascii_lowercase();
        let host = if host == "ssh.github.com" || host == "www.github.com" { DEFAULT_HOST.into() } else { host };
        (valid_host(&host) && valid_owner(owner) && valid_name(name)).then(|| RepoId {
            host,
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }

    /// The repository a git remote URL points at, when it is on a known GitHub host.
    /// Understands `https://`, `ssh://` and scp-style `git@host:owner/name.git` URLs.
    pub fn from_remote(url: &str, config: &GithubConfig) -> Option<RepoId> {
        let url = url.trim();
        let (authority, path) = if let Some((scheme, rest)) = url.split_once("://") {
            if !matches!(scheme, "https" | "http" | "ssh" | "git" | "git+ssh" | "ssh+git") {
                return None;
            }
            rest.split_once('/')?
        } else {
            // scp-style: [user@]host:owner/name
            let (authority, path) = url.split_once(':')?;
            if authority.contains('/') {
                return None;
            }
            (authority, path)
        };
        let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
        let host = host.split_once(':').map_or(host, |(host, _port)| host);
        let mut segments = path.trim_matches('/').split('/');
        let (owner, name) = (segments.next()?, segments.next()?);
        if segments.next().is_some() {
            return None;
        }
        let repo = RepoId::new(host, owner, name)?;
        config.knows_host(&repo.host).then_some(repo)
    }

    /// "owner/name", as GitHub writes it.
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    pub fn is_default_host(&self) -> bool {
        self.host == DEFAULT_HOST
    }

    /// The REST and GraphQL endpoints of this repository's host.
    pub fn api(&self) -> (String, String) {
        api_urls(&self.host)
    }

    pub fn web_url(&self) -> String {
        format!("https://{}/{}/{}", self.host, self.owner, self.name)
    }

    /// Where the GitHub App `slug` is installed on this repository's host.
    pub fn install_url(&self, slug: &str) -> String {
        if self.is_default_host() {
            format!("https://github.com/apps/{slug}/installations/new")
        } else {
            format!("https://{}/github-apps/{slug}/installations/new", self.host)
        }
    }
}

impl fmt::Display for RepoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_default_host() {
            write!(f, "{}/{}", self.owner, self.name)
        } else {
            write!(f, "{}/{}/{}", self.host, self.owner, self.name)
        }
    }
}

/// The REST base and the GraphQL endpoint of `host`: api.github.com for github.com, and
/// GitHub Enterprise Server's `/api/v3` and `/api/graphql` otherwise.
pub fn api_urls(host: &str) -> (String, String) {
    if host.eq_ignore_ascii_case(DEFAULT_HOST) {
        ("https://api.github.com".into(), "https://api.github.com/graphql".into())
    } else {
        (format!("https://{host}/api/v3"), format!("https://{host}/api/graphql"))
    }
}

pub fn valid_host(host: &str) -> bool {
    host.len() <= 253
        && host.contains('.')
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

fn valid_owner(owner: &str) -> bool {
    !owner.is_empty() && owner.len() <= 100 && owner.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name != "."
        && name != ".."
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Whether `client_id` belongs to a GitHub App (its client IDs start with `Iv`) rather than an
/// OAuth app. Only an OAuth app is sent scopes; a GitHub App's permissions come from the app.
pub fn is_github_app(client_id: &str) -> bool {
    client_id.starts_with("Iv")
}

/// A workflow file name or id; it becomes one URL path segment.
pub fn valid_workflow(workflow: &str) -> bool {
    valid_name(workflow.trim())
}

/// What a PR action's placeholders stand for.
#[derive(Debug, Clone, Copy)]
pub struct PrContext<'a> {
    pub number: u64,
    pub head_sha: &'a str,
    pub head: &'a str,
    pub base: &'a str,
}

/// Replaces `{repo}`, `{pr}`, `{head_sha}`, `{head}` and `{base}` in `template`. A PR placeholder
/// without a pull request, or an unknown placeholder, is an error rather than literal text.
pub fn fill(template: &str, repo: &str, pr: Option<&PrContext<'_>>) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let name = &after[..end];
        let pr_value = |value: &dyn Fn(&PrContext<'_>) -> String| {
            pr.map(value).ok_or_else(|| format!("{{{name}}} needs a pull request (scope = \"pr\")"))
        };
        let value = match name {
            "repo" => repo.to_string(),
            "pr" => pr_value(&|pr| pr.number.to_string())?,
            "head_sha" => pr_value(&|pr| pr.head_sha.to_string())?,
            "head" => pr_value(&|pr| pr.head.to_string())?,
            "base" => pr_value(&|pr| pr.base.to_string())?,
            _ => {
                return Err(format!(
                    "unknown placeholder {{{name}}}; use {{repo}} {{pr}} {{head_sha}} {{head}} {{base}}"
                ));
            }
        };
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// An open pull request as the REST list returns it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Pull {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: String,
    pub draft: bool,
    pub head: String,
    pub head_sha: String,
    pub base: String,
    /// Users whose review is requested.
    pub requested: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMethod {
    Squash,
    Merge,
    Rebase,
}

impl MergeMethod {
    pub fn api_name(self) -> &'static str {
        match self {
            MergeMethod::Squash => "squash",
            MergeMethod::Merge => "merge",
            MergeMethod::Rebase => "rebase",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MergeMethod::Squash => "Squash and merge",
            MergeMethod::Merge => "Create a merge commit",
            MergeMethod::Rebase => "Rebase and merge",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CheckState {
    #[default]
    None,
    Pending,
    Passing,
    Failing,
}

impl CheckState {
    /// GitHub's rollup, check run conclusion or status context state.
    pub fn from_github(state: &str) -> CheckState {
        match state.to_ascii_uppercase().as_str() {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => CheckState::Passing,
            "FAILURE" | "ERROR" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE" | "STALE" => {
                CheckState::Failing
            }
            "PENDING" | "EXPECTED" | "QUEUED" | "IN_PROGRESS" | "WAITING" | "REQUESTED" => CheckState::Pending,
            _ => CheckState::None,
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            CheckState::None => "·",
            CheckState::Pending => "●",
            CheckState::Passing => "✓",
            CheckState::Failing => "✗",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Check {
    pub name: String,
    pub state: CheckState,
}

/// What the GraphQL details query says about one pull request.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Details {
    /// MERGEABLE, CONFLICTING or UNKNOWN.
    pub mergeable: String,
    /// CLEAN, BLOCKED, BEHIND, UNSTABLE, HAS_HOOKS, DIRTY, DRAFT or UNKNOWN.
    pub merge_state: String,
    /// APPROVED, CHANGES_REQUESTED, REVIEW_REQUIRED, or none.
    pub review: Option<String>,
    pub checks: CheckState,
    pub check_runs: Vec<Check>,
    /// The viewer may merge past the branch rules.
    pub admin: bool,
}

/// The merge buttons a pull request gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeButtons {
    pub merge: bool,
    pub admin: bool,
}

impl Details {
    pub fn buttons(&self, draft: bool) -> MergeButtons {
        if draft || self.mergeable == "CONFLICTING" {
            return MergeButtons { merge: false, admin: false };
        }
        let state = self.merge_state.as_str();
        MergeButtons {
            merge: matches!(state, "CLEAN" | "HAS_HOOKS" | "UNSTABLE"),
            admin: self.admin && matches!(state, "BLOCKED" | "BEHIND"),
        }
    }

    pub fn review_label(&self) -> &'static str {
        match self.review.as_deref() {
            Some("APPROVED") => "approved",
            Some("CHANGES_REQUESTED") => "changes requested",
            Some("REVIEW_REQUIRED") => "review required",
            _ => "no review needed",
        }
    }
}

/// How long to wait before polling again after `status`, or `None` when it was no rate limit.
/// `retry_after` and `reset_in` are seconds from the response's headers; `previous` is the last wait.
pub fn backoff(
    status: u16,
    remaining: Option<u64>,
    retry_after: Option<u64>,
    reset_in: Option<u64>,
    previous: Option<u64>,
) -> Option<u64> {
    let limited = status == 429 || (status == 403 && (remaining == Some(0) || retry_after.is_some()));
    if !limited {
        return None;
    }
    let wait = if let Some(seconds) = retry_after {
        seconds
    } else if remaining == Some(0)
        && let Some(seconds) = reset_in
    {
        seconds
    } else {
        previous.map_or(60, |last| last.saturating_mul(2)).clamp(60, 15 * 60)
    };
    Some(wait.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(hosts: &[&str]) -> GithubConfig {
        GithubConfig { hosts: hosts.iter().map(|h| h.to_string()).collect(), ..GithubConfig::default() }
    }

    #[test]
    fn github_remotes_of_every_url_shape_on_known_hosts_only() {
        let config = config(&["git.example.com"]);
        let repo = |host: &str, owner: &str, name: &str| {
            Some(RepoId { host: host.into(), owner: owner.into(), name: name.into() })
        };
        for (url, expected) in [
            ("https://github.com/owner/name.git", repo("github.com", "owner", "name")),
            ("https://user@github.com/owner/name", repo("github.com", "owner", "name")),
            ("git@github.com:owner/name.git", repo("github.com", "owner", "name")),
            ("ssh://git@ssh.github.com:443/owner/name.git", repo("github.com", "owner", "name")),
            ("ssh://git@git.example.com/team/tool.git", repo("git.example.com", "team", "tool")),
            ("https://GIT.example.com/team/tool/", repo("git.example.com", "team", "tool")),
            ("https://gitlab.com/owner/name.git", None),
            ("https://github.com/owner/name/extra", None),
            ("/srv/git/name.git", None),
            ("file:///srv/git/name.git", None),
            ("https://github.com/owner/..", None),
        ] {
            assert_eq!(RepoId::from_remote(url, &config), expected, "{url}");
        }
        assert_eq!(RepoId::parse("owner/name").unwrap().to_string(), "owner/name");
        let enterprise = RepoId::parse("git.example.com/team/tool").unwrap();
        assert_eq!(enterprise.to_string(), "git.example.com/team/tool");
        assert_eq!(enterprise.api().0, "https://git.example.com/api/v3");
        assert_eq!(enterprise.api().1, "https://git.example.com/api/graphql");
        assert_eq!(RepoId::parse("owner/name").unwrap().api().0, "https://api.github.com");
        assert!(RepoId::parse("a/b/c/d").is_none() && RepoId::parse("owner/na me").is_none());
    }

    #[test]
    fn github_placeholders_fill_or_fail_by_scope() {
        let pr = PrContext { number: 42, head_sha: "abc123", head: "feature", base: "main" };
        assert_eq!(
            fill("{repo}#{pr} {head}@{head_sha} → {base}", "owner/name", Some(&pr)).unwrap(),
            "owner/name#42 feature@abc123 → main"
        );
        assert_eq!(fill("v{repo}", "owner/name", None).unwrap(), "vowner/name");
        assert!(fill("{pr}", "owner/name", None).unwrap_err().contains("needs a pull request"));
        assert!(fill("{nope}", "owner/name", Some(&pr)).unwrap_err().contains("unknown placeholder {nope}"));
        assert_eq!(fill("open {brace", "o/n", None).unwrap(), "open {brace");
    }

    #[test]
    fn github_config_parses_and_reports_problems() {
        let config: GithubConfig = toml::from_str(
            r#"
hosts = ["git.example.com", "not a host"]
repos = ["owner/name", "bad"]
filter = "review"
[[actions]]
label = "Build"
repo = "owner/name"
workflow = "build.yml"
inputs = { sha = "{head_sha}", note = "" }
[[actions]]
label = "Nightly"
workflow = "../escape"
scope = "repo"
ref = "{base}"
"#,
        )
        .unwrap();
        assert_eq!(config.filter, Filter::Review);
        assert_eq!(config.actions[0].ref_template(), Some("{base}"));
        assert_eq!(config.actions[1].scope, Scope::Repo);
        let problems = config.problems();
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(problems[0].contains("not a host") && problems[1].contains("\"bad\""));
        assert!(problems.iter().any(|p| p.contains("Nightly: workflow")));
        assert!(problems.iter().any(|p| p.contains("Nightly: {base} needs a pull request")));
        assert!(config.actions[0].applies_to(&RepoId::parse("owner/name").unwrap()));
        assert!(!config.actions[0].applies_to(&RepoId::parse("owner/other").unwrap()));
        assert!(config.actions[1].applies_to(&RepoId::parse("owner/other").unwrap()));
        assert_eq!(GithubConfig::default().poll_interval(), 60);
        let own = GithubConfig { client_id: "Iv1.enterprise".into(), ..GithubConfig::default() };
        assert_eq!(own.client_id(), "Iv1.enterprise");
        assert_eq!(GithubConfig::default().client_id(), DEFAULT_CLIENT_ID);
        assert!(is_github_app(DEFAULT_CLIENT_ID), "the ATOMICS app is a GitHub App: no scopes");
        assert!(is_github_app("Iv1.enterprise") && is_github_app("Iv23liApp") && !is_github_app("Ov23liOauthApp"));
        assert_eq!(GithubConfig::default().app_slug(), "atomiskide");
        let app = GithubConfig { app_slug: "my-app".into(), ..GithubConfig::default() };
        assert_eq!(app.app_slug(), "my-app");
        assert_eq!(
            RepoId::parse("octo/widgets").unwrap().install_url("my-app"),
            "https://github.com/apps/my-app/installations/new"
        );
        assert_eq!(
            RepoId::parse("git.example.com/team/tool").unwrap().install_url("my-app"),
            "https://git.example.com/github-apps/my-app/installations/new"
        );
        assert_eq!(GithubConfig { poll_seconds: 1, ..GithubConfig::default() }.poll_interval(), 15);
    }

    #[test]
    fn github_filter_and_merge_buttons() {
        let pull = Pull { author: "Me".into(), requested: vec!["reviewer".into()], ..Pull::default() };
        assert!(Filter::Mine.matches(&pull, "me") && !Filter::Mine.matches(&pull, "reviewer"));
        assert!(Filter::Review.matches(&pull, "reviewer") && !Filter::Review.matches(&pull, "me"));
        assert!(Filter::All.matches(&pull, "anyone"));
        let details = |state: &str, admin: bool| Details { merge_state: state.into(), admin, ..Details::default() };
        let buttons = |merge, admin| MergeButtons { merge, admin };
        assert_eq!(details("CLEAN", true).buttons(false), buttons(true, false));
        assert_eq!(details("UNSTABLE", false).buttons(false), buttons(true, false));
        assert_eq!(details("BLOCKED", false).buttons(false), buttons(false, false));
        assert_eq!(details("BLOCKED", true).buttons(false), buttons(false, true));
        assert_eq!(details("BEHIND", true).buttons(false), buttons(false, true));
        assert_eq!(details("BLOCKED", true).buttons(true), buttons(false, false), "drafts never merge");
        let conflicting = Details { mergeable: "CONFLICTING".into(), ..details("DIRTY", true) };
        assert_eq!(conflicting.buttons(false), buttons(false, false));
        assert_eq!(CheckState::from_github("failure"), CheckState::Failing);
        assert_eq!(CheckState::from_github("IN_PROGRESS"), CheckState::Pending);
    }

    #[test]
    fn github_backoff_follows_headers_then_doubles() {
        assert_eq!(backoff(200, None, None, None, None), None);
        assert_eq!(backoff(403, Some(10), None, None, None), None, "a permission error is no rate limit");
        assert_eq!(backoff(403, Some(0), None, Some(120), None), Some(120));
        assert_eq!(backoff(429, None, Some(7), None, None), Some(7));
        assert_eq!(backoff(429, None, None, None, None), Some(60));
        assert_eq!(backoff(429, None, None, None, Some(60)), Some(120));
        assert_eq!(backoff(429, None, None, None, Some(800)), Some(900));
        assert_eq!(backoff(403, None, Some(0), None, None), Some(1));
    }
}
