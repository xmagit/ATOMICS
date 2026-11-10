//! The pull request controls for GitHub and Azure DevOps: the top-bar counter, the controls modal,
//! Connect GitHub, merges, workflow and pipeline runs, and pull request diffs in the diff viewer.
//! The network, `gh`, `az` and the keychain are used on worker threads (`crate::github`); the state
//! keeps what they report, never a token except the one `gh` offered while "Use your gh account?"
//! waits for an answer.
use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::credentials::Store as CredentialStore;
use crate::github::{self as gh, ApiError, Endpoints, RepoDetails, Run, Token};
use atomics_core::github::{AZURE_HOST, Action, CheckState, Group, MergeMethod, PrContext, Pull, RepoId, Scope};

/// A workflow's latest run, `None` when it never ran, or why it could not be read.
type RunStatus = Result<Option<Run>, String>;

#[derive(Default)]
pub(super) struct Github {
    repos: Vec<RepoId>,
    lists: BTreeMap<RepoId, Listing>,
    /// Host → the signed-in login.
    viewers: BTreeMap<String, String>,
    /// Hosts without a stored token.
    signed_out: BTreeSet<String>,
    /// When the next poll is due; `None` polls on the next tick.
    next_poll: Option<Instant>,
    /// The last rate-limit wait, doubled while limits continue.
    backoff: Option<u64>,
    polling: bool,
    ruleset_cache: Arc<Mutex<gh::RulesetCache>>,
    /// Azure DevOps's token, viewer and team memberships, shared by polls and actions.
    azure: gh::azure::Shared,
    /// The modal is open: polls fetch details, and it shows.
    pub(super) modal: bool,
    jobs: Vec<Receiver<Done>>,
    pub(super) connect: Connect,
    /// Pull requests with a merge under way.
    busy: BTreeSet<(RepoId, u64)>,
    /// Actions being dispatched, by repository and label.
    dispatching: BTreeSet<(RepoId, String)>,
    runs: BTreeMap<(RepoId, String), RunStatus>,
    /// Per host, listed repositories the GitHub App is not installed on.
    uncovered: BTreeMap<String, Vec<RepoId>>,
    /// The last poll's problem (a rate limit, an unreachable host).
    error: Option<String>,
    /// The host just signed out of: the modal links to GitHub's page for revoking the app fully.
    pub(super) revoke: Option<String>,
    /// The outcome of the last action, shown in the modal.
    pub(super) notice: Option<String>,
    pub(super) pr_diff: Option<PrDiff>,
    pr_diffs: BTreeMap<String, PrDiff>,
    diff_revision: u64,
    /// The head commit each pull request's diff was loaded at. Merge sends it, so GitHub refuses
    /// (409) a merge whose branch moved after the diff was reviewed.
    reviewed: BTreeMap<(RepoId, u64), String>,
    /// Tests reach a fake GitHub, a fake `gh` and a stand-in browser.
    pub(super) endpoints: Endpoints,
    pub(super) browser: Option<crate::connections::Browser>,
}

#[derive(Default, Clone)]
struct Listing {
    pulls: Vec<Pull>,
    etag: Option<String>,
    details: Option<RepoDetails>,
    error: Option<String>,
}

/// Connect GitHub, from the button to the "GitHub · <login>" chip.
#[derive(Default)]
pub(super) enum Connect {
    #[default]
    Idle,
    /// Asking `gh` for its token.
    Checking,
    /// `gh` is signed in as `login`: "Use your gh account <login>?"
    ConfirmGh {
        host: String,
        login: String,
        token: Token,
    },
    /// The device flow: the code is on the clipboard and the page is open in the browser.
    Device {
        user_code: String,
        uri: String,
        copied: bool,
        cancel: Arc<AtomicBool>,
    },
    Failed(String),
}

/// A pull request's diff, shown read-only in the diff viewer.
#[derive(Clone)]
pub(super) struct PrDiff {
    pub revision: u64,
    pub repo: RepoId,
    pub number: u64,
    pub title: String,
    pub head: String,
    pub base: String,
    /// The head commit the diff on screen was loaded at.
    pub head_sha: Option<String>,
    pub prepared: Option<Result<Arc<diff_view::Prepared>, String>>,
    /// The session and worktree it was opened over; selecting another one closes it.
    pub over: (Option<String>, Option<PathBuf>),
}

enum Done {
    Polled(gh::Polled),
    GhChecked {
        host: String,
        found: Option<(Token, Result<String, ApiError>)>,
        client_id: String,
    },
    DeviceCode {
        result: Result<gh::DeviceCode, String>,
        cancel: Arc<AtomicBool>,
    },
    Connected {
        host: String,
        result: Result<String, String>,
    },
    Disconnected {
        host: String,
        result: Result<(), String>,
    },
    Merged {
        repo: RepoId,
        number: u64,
        admin: bool,
        result: Result<String, String>,
    },
    Dispatched {
        repo: RepoId,
        label: String,
        result: Result<String, String>,
    },
    Runs(Vec<((RepoId, String), RunStatus)>),
    /// The diff and the head commit it was loaded at.
    Diff {
        repo: RepoId,
        number: u64,
        revision: u64,
        result: Result<(Arc<diff_view::Prepared>, String), String>,
    },
}

/// What the top-bar counter shows.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Counter {
    Hidden,
    SignedOut,
    Count { count: usize, failing: bool },
}

/// A pull request as the modal lists it.
#[derive(Clone)]
pub(super) struct Row {
    pub repo: RepoId,
    pub pull: Pull,
    pub details: Option<atomics_core::github::Details>,
    pub counted: bool,
    pub group: Group,
}

impl Github {
    fn store(relay: &crate::relay::Service) -> CredentialStore {
        CredentialStore::new(relay.keychain().clone())
    }

    /// The host Connect GitHub signs in to: the first listed repository's that has no token.
    /// Azure DevOps signs in with `az login` instead.
    pub(super) fn connect_host(&self) -> String {
        self.signed_out
            .iter()
            .find(|host| *host != AZURE_HOST)
            .cloned()
            .or_else(|| self.repos.iter().find(|repo| !repo.is_azure()).map(|repo| repo.host.clone()))
            .unwrap_or_else(|| atomics_core::github::DEFAULT_HOST.into())
    }

    pub(super) fn viewer(&self, host: &str) -> Option<&str> {
        self.viewers.get(host).map(String::as_str)
    }

    pub(super) fn signed_in_hosts(&self) -> Vec<(String, String)> {
        self.viewers.iter().map(|(host, login)| (host.clone(), login.clone())).collect()
    }

    /// Connect GitHub is offered: a GitHub host has no token, or nothing is listed yet.
    pub(super) fn needs_connect(&self) -> bool {
        self.signed_out.iter().any(|host| host != AZURE_HOST) || (self.repos.is_empty() && self.viewers.is_empty())
    }

    /// Azure DevOps repositories are listed, but the Azure CLI is not signed in.
    pub(super) fn needs_az_login(&self) -> bool {
        self.signed_out.contains(AZURE_HOST)
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub(super) fn repos(&self) -> &[RepoId] {
        &self.repos
    }

    pub(super) fn repo_error(&self, repo: &RepoId) -> Option<&str> {
        self.lists.get(repo).and_then(|listing| listing.error.as_deref())
    }

    pub(super) fn methods(&self, repo: &RepoId) -> Vec<MergeMethod> {
        self.lists.get(repo).and_then(|listing| listing.details.as_ref()).map(|d| d.methods.clone()).unwrap_or_default()
    }

    pub(super) fn run(&self, repo: &RepoId, label: &str) -> Option<&RunStatus> {
        self.runs.get(&(repo.clone(), label.to_string()))
    }

    pub(super) fn is_busy(&self, repo: &RepoId, number: u64) -> bool {
        self.busy.contains(&(repo.clone(), number))
    }

    /// One entry per account the app is missing on: the account and where to install the app.
    pub(super) fn installs(&self, slug: &str) -> Vec<(String, String)> {
        let mut seen = BTreeSet::new();
        let mut installs = Vec::new();
        for repo in self.uncovered.values().flatten() {
            if seen.insert((repo.host.clone(), repo.owner.to_ascii_lowercase())) {
                installs.push((repo.owner.clone(), repo.install_url(slug)));
            }
        }
        installs
    }

    /// "Merge pull request?" for `row`, sending the head its diff was loaded at when it was, else the
    /// head the list shows. GitHub refuses (409) a merge whose head has moved past that commit.
    pub(super) fn merge_confirm(&self, row: &Row, method: MergeMethod, admin: bool) -> Confirm {
        let reviewed = self.reviewed.get(&(row.repo.clone(), row.pull.number));
        Confirm::Merge {
            repo: row.repo.clone(),
            number: row.pull.number,
            title: row.pull.title.clone(),
            method,
            sha: reviewed.cloned().unwrap_or_else(|| row.pull.head_sha.clone()),
            latest: row.pull.head_sha.clone(),
            reviewed: reviewed.is_some(),
            admin,
        }
    }

    pub(super) fn is_dispatching(&self, repo: &RepoId, label: &str) -> bool {
        self.dispatching.contains(&(repo.clone(), label.to_string()))
    }

    /// Every open pull request, by repository, with what the counter's filter says of it. Azure
    /// DevOps repositories, where a team lists many, are grouped: assigned to the viewer, the
    /// viewer's own, then the rest.
    pub(super) fn rows(&self, filter: atomics_core::github::Filter) -> Vec<Row> {
        let mut rows = Vec::new();
        for repo in &self.repos {
            let Some(listing) = self.lists.get(repo) else { continue };
            let viewer = self.viewers.get(&repo.host).map(String::as_str).unwrap_or_default();
            let start = rows.len();
            for pull in &listing.pulls {
                rows.push(Row {
                    repo: repo.clone(),
                    pull: pull.clone(),
                    details: listing.details.as_ref().and_then(|details| details.pulls.get(&pull.number).cloned()),
                    counted: !viewer.is_empty() && filter.matches(pull, viewer),
                    group: if repo.is_azure() { Group::of(pull, viewer) } else { Group::Other },
                });
            }
            rows[start..].sort_by_key(|row| row.group);
        }
        rows
    }

    pub(super) fn counter(&self, config: &atomics_core::github::GithubConfig) -> Counter {
        if !config.enabled || self.repos.is_empty() {
            return Counter::Hidden;
        }
        if self.viewers.is_empty() {
            return Counter::SignedOut;
        }
        let counted: Vec<Row> = self.rows(config.filter).into_iter().filter(|row| row.counted).collect();
        let failing = counted.iter().any(|row| row.details.as_ref().is_some_and(|d| d.checks == CheckState::Failing));
        Counter::Count { count: counted.len(), failing }
    }
}

impl State {
    fn github_job(&mut self, job: impl FnOnce(&crate::host::Sender<Done>) + Send + 'static) {
        let (tx, rx) = self.wake.channel();
        self.github.jobs.push(rx);
        std::thread::spawn(move || job(&tx));
    }

    /// For a worker: the host's client, with GitHub's stored token or the Azure CLI's. An expired
    /// or refused GitHub token is removed, and the next poll shows Connect GitHub.
    fn github_api(&self, host: &str) -> impl FnOnce() -> Result<gh::Client, String> + Send + 'static {
        let (endpoints, store, host) = (self.github.endpoints.clone(), Github::store(&self.relay), host.to_string());
        let azure = self.github.azure.clone();
        move || {
            gh::Client::open(&endpoints, &store, &azure, &host).map_err(|error| error.to_string())?.ok_or_else(|| {
                if host == AZURE_HOST {
                    "Azure DevOps is not signed in; run az login".to_string()
                } else {
                    "GitHub is not connected; press Connect GitHub".to_string()
                }
            })
        }
    }

    fn github_browser(&self) -> crate::connections::Browser {
        self.github.browser.clone().unwrap_or_else(crate::connections::Browser::current)
    }

    /// Opens `url` in the browser off this thread; `open` may wait for some handlers.
    pub(super) fn github_open_url(&mut self, url: String) {
        let browser = self.github_browser();
        std::thread::spawn(move || {
            let _ = browser.open(&url);
        });
    }

    /// Starts a poll when one is due, and applies what the workers finished.
    pub(super) fn github_tick(&mut self) -> bool {
        let changed = self.poll_github_jobs();
        if !self.config.github.enabled || self.github.polling {
            return changed;
        }
        if self.github.next_poll.is_some_and(|at| at > Instant::now()) {
            return changed;
        }
        self.github.polling = true;
        let request = gh::PollRequest {
            projects: self.repos.clone(),
            config: self.config.github.clone(),
            etags: self
                .github
                .lists
                .iter()
                .filter_map(|(repo, listing)| Some((repo.clone(), listing.etag.clone()?)))
                .collect(),
            viewers: self.github.viewers.keys().cloned().collect(),
            details: self.github.modal,
            ruleset_cache: self.github.ruleset_cache.clone(),
            azure: self.github.azure.clone(),
        };
        let (endpoints, store) = (self.github.endpoints.clone(), Github::store(&self.relay));
        self.github_job(move |tx| {
            let _ = tx.send(Done::Polled(gh::poll(&endpoints, &store, &request)));
        });
        changed
    }

    /// Polls on the next tick, with details when the modal is open.
    pub(super) fn github_poll_now(&mut self) {
        if self.github.backoff.is_none() {
            self.github.next_poll = None;
        }
    }

    pub(super) fn github_set_modal(&mut self, open: bool) {
        if open && !self.github.modal {
            self.github.modal = true;
            self.github.notice = None;
            self.github_poll_now();
            self.github_runs();
        } else if !open {
            self.github.modal = false;
            if let Connect::Device { cancel, .. } = &self.github.connect {
                cancel.store(true, Ordering::Relaxed);
            }
            if !matches!(self.github.connect, Connect::Idle) {
                self.github.connect = Connect::Idle;
            }
        }
    }

    fn poll_github_jobs(&mut self) -> bool {
        let mut changed = false;
        for rx in std::mem::take(&mut self.github.jobs) {
            let mut keep = true;
            loop {
                match rx.try_recv() {
                    Ok(done) => {
                        changed = true;
                        self.github_done(done);
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        keep = false;
                        break;
                    }
                }
            }
            if keep {
                self.github.jobs.push(rx);
            }
        }
        changed
    }

    fn github_done(&mut self, done: Done) {
        match done {
            Done::Polled(polled) => self.github_polled(polled),
            Done::GhChecked { host, found, client_id } => match found {
                Some((token, Ok(login))) => self.github.connect = Connect::ConfirmGh { host, login, token },
                Some((_, Err(ApiError::Unauthorized))) | None => self.github_device_flow(host, client_id),
                Some((_, Err(error))) => self.github.connect = Connect::Failed(error.to_string()),
            },
            Done::DeviceCode { result, cancel } => match result {
                Ok(code) => {
                    self.github.connect =
                        Connect::Device { user_code: code.user_code, uri: code.verification_uri, copied: false, cancel }
                }
                Err(error) => self.github.connect = Connect::Failed(error),
            },
            Done::Connected { host, result } => match result {
                Ok(login) => {
                    self.github.connect = Connect::Idle;
                    self.github.revoke = None;
                    self.github.signed_out.remove(&host);
                    self.github.viewers.insert(host, login.clone());
                    self.github.lists.values_mut().for_each(|listing| listing.etag = None);
                    self.github.backoff = None;
                    self.github_poll_now();
                    self.github_runs();
                    self.toast(format!("GitHub · {login} connected"));
                }
                Err(error) => self.github.connect = Connect::Failed(error),
            },
            Done::Disconnected { host, result } => match result {
                Ok(()) => {
                    self.github.viewers.remove(&host);
                    self.github.signed_out.insert(host.clone());
                    self.github.revoke = Some(host.clone());
                    self.github.lists.retain(|repo, _| repo.host != host);
                    self.toast(format!("Disconnected GitHub on {host}"));
                }
                Err(error) => self.toast(format!("Could not disconnect GitHub: {error}")),
            },
            Done::Merged { repo, number, admin, result } => {
                self.github.busy.remove(&(repo.clone(), number));
                let what = if admin { "Admin merged" } else { "Merged" };
                let notice = match result {
                    Ok(_) => format!("{what} {repo}#{number}"),
                    Err(error) => format!("Merging {repo}#{number} failed: {error}"),
                };
                self.toast(notice.clone());
                self.github.notice = Some(notice);
                if let Some(listing) = self.github.lists.get_mut(&repo) {
                    listing.etag = None;
                }
                self.github_poll_now();
            }
            Done::Dispatched { repo, label, result } => {
                self.github.dispatching.remove(&(repo.clone(), label.clone()));
                let notice = match result {
                    Ok(git_ref) => format!("Started {label} on {repo} at {git_ref}"),
                    Err(error) => {
                        // A refused token was removed; the poll shows Connect GitHub.
                        self.github_poll_now();
                        format!("{label} on {repo} failed: {error}")
                    }
                };
                self.toast(notice.clone());
                self.github.notice = Some(notice);
            }
            Done::Runs(runs) => self.github.runs.extend(runs),
            Done::Diff { repo, number, revision, result } => {
                let key = pr_key(&repo, number);
                if let Some(view) = self.github.pr_diffs.get_mut(&key).filter(|v| v.revision == revision) {
                    match result {
                        Ok((prepared, sha)) => {
                            view.head_sha = Some(sha.clone());
                            view.prepared = Some(Ok(prepared));
                            self.github.reviewed.insert((repo.clone(), number), sha);
                        }
                        Err(error) => view.prepared = Some(Err(error)),
                    }
                    if self.github.pr_diff.as_ref().is_some_and(|v| v.repo == repo && v.number == number) {
                        self.github.pr_diff = Some(view.clone());
                    }
                }
            }
        }
    }

    fn github_polled(&mut self, polled: gh::Polled) {
        self.github.polling = false;
        self.github.repos = polled.repos;
        let repos: BTreeSet<&RepoId> = self.github.repos.iter().collect();
        self.github.lists.retain(|repo, _| repos.contains(repo));
        for host in &polled.signed_out {
            self.github.viewers.remove(host);
        }
        self.github.signed_out = polled.signed_out;
        if polled.checked {
            self.github.uncovered = polled.uncovered;
        }
        let signed_out = &self.github.signed_out;
        self.github.uncovered.retain(|host, _| !signed_out.contains(host));
        let mut limited = None;
        let mut error = None;
        let mut note = |failure: &ApiError, host: &str, viewers: &mut BTreeMap<String, String>| {
            if let Some(wait) = failure.backoff(self.github.backoff) {
                limited = Some(wait);
            } else if *failure == ApiError::Unauthorized {
                viewers.remove(host);
            }
            error = Some(failure.to_string());
        };
        for (host, viewer) in polled.viewers {
            match viewer {
                Ok(login) => {
                    self.github.viewers.insert(host, login);
                }
                Err(failure) => note(&failure, &host, &mut self.github.viewers),
            }
        }
        for (repo, list) in polled.lists {
            let listing = self.github.lists.entry(repo.clone()).or_default();
            match list {
                Ok(Some((pulls, etag))) => {
                    listing.pulls = pulls;
                    listing.etag = etag;
                    listing.error = None;
                }
                Ok(None) => listing.error = None,
                Err(failure) => {
                    listing.error = Some(failure.to_string());
                    note(&failure, &repo.host, &mut self.github.viewers);
                }
            }
        }
        // A reviewed head is kept only while its pull request is still open.
        let lists = &self.github.lists;
        self.github
            .reviewed
            .retain(|(repo, number), _| lists.get(repo).is_some_and(|l| l.pulls.iter().any(|p| p.number == *number)));
        for (repo, details) in polled.details {
            let listing = self.github.lists.entry(repo).or_default();
            match details {
                Ok(details) => listing.details = Some(details),
                Err(failure) => listing.error = Some(failure.to_string()),
            }
        }
        // A refused token is signed out; Connect GitHub replaces it.
        for repo in &self.github.repos {
            if !self.github.viewers.contains_key(&repo.host) {
                self.github.signed_out.insert(repo.host.clone());
            }
        }
        self.github.backoff = limited;
        self.github.error = error;
        let wait = limited.unwrap_or_else(|| self.config.github.poll_interval());
        self.github.next_poll = Some(Instant::now() + Duration::from_secs(wait));
    }

    /// Connect GitHub: `gh`'s account when it is signed in, else the device flow.
    pub(super) fn github_connect(&mut self) {
        if matches!(self.github.connect, Connect::Checking | Connect::Device { .. }) {
            return;
        }
        let host = self.github.connect_host();
        self.github.connect = Connect::Checking;
        let endpoints = self.github.endpoints.clone();
        let client_id = self.config.github.client_id().to_string();
        self.github_job(move |tx| {
            let found = gh::gh_token(endpoints.gh.as_deref(), &host).map(|token| {
                let login = endpoints.api(&host, Some(token.clone())).viewer();
                (token, login)
            });
            let _ = tx.send(Done::GhChecked { host, found, client_id });
        });
    }

    /// The device flow with the configured app. A token that expires is stored with its expiry and
    /// never refreshed: an expired token means Connect GitHub again.
    fn github_device_flow(&mut self, host: String, client_id: String) {
        self.github.connect = Connect::Checking;
        let (endpoints, store, browser) =
            (self.github.endpoints.clone(), Github::store(&self.relay), self.github_browser());
        let cancel = Arc::new(AtomicBool::new(false));
        self.github_job(move |tx| {
            let api = endpoints.api(&host, None);
            let code = match api.device_code(&client_id) {
                Ok(code) => code,
                Err(error) => {
                    let _ = tx.send(Done::DeviceCode { result: Err(error.to_string()), cancel });
                    return;
                }
            };
            let _ = browser.open(&code.verification_uri);
            let _ = tx.send(Done::DeviceCode { result: Ok(code.clone()), cancel: cancel.clone() });
            let deadline = Instant::now() + Duration::from_secs(code.expires_in);
            let mut interval = code.interval;
            let result = loop {
                // Sleep in short steps so Cancel and closing the modal stop the flow promptly.
                let wake = Instant::now() + Duration::from_secs(interval);
                while Instant::now() < wake {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(if cfg!(test) { 10 } else { 200 }));
                }
                if Instant::now() > deadline {
                    break Err("The code expired; press Connect GitHub again".to_string());
                }
                match api.device_token(&client_id, &code.device_code) {
                    Ok(gh::DevicePoll::Pending) => {}
                    Ok(gh::DevicePoll::SlowDown) => interval += 5,
                    Ok(gh::DevicePoll::Failed(error)) => break Err(error),
                    Err(error) => break Err(error.to_string()),
                    Ok(gh::DevicePoll::Token(grant)) => {
                        let now = SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
                        break store
                            .put_github_app_grant(&host, &grant.stored(now))
                            .map_err(|error| error.to_string())
                            .and_then(|()| {
                                endpoints.api(&host, Some(grant.token)).viewer().map_err(|e| e.to_string())
                            });
                    }
                }
            };
            let _ = tx.send(Done::Connected { host, result });
        });
    }

    /// The answer to "Use your gh account <login>?": yes stores its token, no starts the device flow.
    pub(super) fn github_use_gh(&mut self, accept: bool) {
        let Connect::ConfirmGh { host, login, token } = std::mem::take(&mut self.github.connect) else { return };
        if !accept {
            let client_id = self.config.github.client_id().to_string();
            self.github_device_flow(host, client_id);
            return;
        }
        self.github.connect = Connect::Checking;
        let store = Github::store(&self.relay);
        self.github_job(move |tx| {
            let result = store.put_github_app(&host, token.secret()).map(|()| login).map_err(|error| error.to_string());
            let _ = tx.send(Done::Connected { host, result });
        });
    }

    pub(super) fn github_cancel_connect(&mut self) {
        if let Connect::Device { cancel, .. } = &self.github.connect {
            cancel.store(true, Ordering::Relaxed);
        }
        self.github.connect = Connect::Idle;
    }

    pub(super) fn github_disconnect(&mut self, host: String) {
        let store = Github::store(&self.relay);
        self.github_job(move |tx| {
            let result = store.remove_github_app(&host).map_err(|error| error.to_string());
            let _ = tx.send(Done::Disconnected { host, result });
        });
    }

    /// Merges after the user confirmed; admin merge is the same call, offered only when GitHub
    /// says the viewer may bypass the rules.
    pub(super) fn github_merge(&mut self, repo: RepoId, number: u64, method: MergeMethod, sha: String, admin: bool) {
        if !self.github.busy.insert((repo.clone(), number)) {
            return;
        }
        let api = self.github_api(&repo.host);
        self.github_job(move |tx| {
            let result = api().and_then(|mut client| {
                client.merge(&repo, number, method, &sha, admin).map_err(|error| error.to_string())
            });
            let _ = tx.send(Done::Merged { repo, number, admin, result });
        });
    }

    /// Dispatches `action` after the user confirmed, with its ref and inputs already filled in;
    /// no ref runs on the default branch.
    pub(super) fn github_dispatch(
        &mut self,
        repo: RepoId,
        action: Action,
        git_ref: Option<String>,
        inputs: BTreeMap<String, String>,
    ) {
        if !self.github.dispatching.insert((repo.clone(), action.label.clone())) {
            return;
        }
        let api = self.github_api(&repo.host);
        self.github_job(move |tx| {
            let key = (repo.clone(), action.label.clone());
            let result = api().and_then(|mut client| {
                let git_ref = match git_ref {
                    Some(git_ref) => git_ref,
                    None => client.default_branch(&repo).map_err(|error| error.to_string())?,
                };
                client.dispatch(&repo, &action.workflow, &git_ref, &inputs).map_err(|error| error.to_string())?;
                let run = client.latest_run(&repo, &action.workflow).map_err(|error| error.to_string());
                let _ = tx.send(Done::Runs(vec![(key, run)]));
                Ok(git_ref)
            });
            let _ = tx.send(Done::Dispatched { repo, label: action.label, result });
        });
    }

    /// The latest run of every configured action on every listed repository it applies to.
    pub(super) fn github_runs(&mut self) {
        let mut wanted: BTreeMap<String, Vec<(RepoId, Action)>> = BTreeMap::new();
        for repo in &self.github.repos {
            for action in self.config.github.actions.iter().filter(|action| action.applies_to(repo)) {
                if self.github.viewers.contains_key(&repo.host) {
                    wanted.entry(repo.host.clone()).or_default().push((repo.clone(), action.clone()));
                }
            }
        }
        for (host, actions) in wanted {
            let api = self.github_api(&host);
            self.github_job(move |tx| {
                let Ok(mut client) = api() else { return };
                let runs = actions
                    .into_iter()
                    .map(|(repo, action)| {
                        let run = client.latest_run(&repo, &action.workflow).map_err(|error| error.to_string());
                        ((repo, action.label), run)
                    })
                    .collect();
                let _ = tx.send(Done::Runs(runs));
            });
        }
    }

    /// Loads a pull request's diff from GitHub or Azure DevOps into the diff viewer, read-only. No
    /// fetch, ref or worktree is touched.
    pub(super) fn github_view_diff(&mut self, repo: RepoId, pull: &Pull) {
        self.fetch_pull_request(repo, pull, true);
    }

    fn fetch_pull_request(&mut self, repo: RepoId, pull: &Pull, select: bool) {
        self.github.diff_revision += 1;
        let revision = self.github.diff_revision;
        let view = PrDiff {
            revision,
            repo: repo.clone(),
            number: pull.number,
            title: pull.title.clone(),
            head: pull.head.clone(),
            base: pull.base.clone(),
            head_sha: None,
            prepared: None,
            over: (self.active.clone(), self.diff_root.clone()),
        };
        let key = pr_key(&repo, pull.number);
        if select {
            self.github.pr_diff = Some(view.clone());
        }
        self.github.pr_diffs.insert(key.clone(), view);
        // Keep the host cache bounded. Selection and scroll remain on each client.
        if self.github.pr_diffs.len() > 16 {
            let oldest = self
                .github
                .pr_diffs
                .iter()
                .filter(|(k, _)| **k != key)
                .min_by_key(|(_, v)| v.revision)
                .map(|(k, _)| k.clone());
            if let Some(oldest) = oldest {
                self.github.pr_diffs.remove(&oldest);
            }
        }
        if select {
            self.client_events.push(ClientEvent::PullRequest(key));
            self.diff_open = true;
            self.panel = PanelMode::Diff;
        }
        let (api, highlighter, number) = (self.github_api(&repo.host), self.highlighter.clone(), pull.number);
        self.github_job(move |tx| {
            let result = api().and_then(|mut client| {
                // The head first, then the diff: a push in between leaves the diff newer than the
                // recorded head, so a merge of that head fails instead of merging unreviewed code.
                let sha = client.head(&repo, number).map_err(|error| error.to_string())?;
                let text = client.diff(&repo, number).map_err(|error| error.to_string())?;
                let mut files = atomics_core::diff::parse_unified(&text);
                files.sort_by(|a, b| a.path().cmp(b.path()));
                let diff = atomics_core::diff::WorktreeDiff {
                    base_ref: format!("{repo}#{number}"),
                    base: String::new(),
                    files,
                    partial: false,
                };
                Ok((crate::diff_worker::prepare_diff(diff, &highlighter), sha))
            });
            let _ = tx.send(Done::Diff { repo, number, revision, result });
        });
    }

    /// The pull request diff on screen, if one is open over the current selection.
    pub(super) fn github_pr_diff(&mut self) -> Option<&PrDiff> {
        let over = (self.active.clone(), self.diff_root.clone());
        if self.github.pr_diff.as_ref().is_some_and(|view| view.over != over) {
            self.github.pr_diff = None;
        }
        self.github.pr_diff.as_ref()
    }
}

/// What the user is asked to confirm before something changes on GitHub.
#[derive(Clone)]
pub(super) enum Confirm {
    Merge {
        repo: RepoId,
        number: u64,
        title: String,
        method: MergeMethod,
        /// The head commit sent with the merge: the one whose diff was loaded, else the listed one.
        sha: String,
        /// The head the list shows now; when it is not `sha`, GitHub will refuse the merge.
        latest: String,
        /// `sha` is the commit whose diff was loaded in the diff viewer.
        reviewed: bool,
        admin: bool,
    },
    Dispatch {
        repo: RepoId,
        action: Action,
        git_ref: Option<String>,
        inputs: BTreeMap<String, String>,
        asked: Vec<String>,
    },
}

/// The modal's own state: the selection, the method picked per repository, the confirmation on
/// screen, and the diff viewer's state for the pull request diff.
#[derive(Default)]
pub(super) struct GithubUi {
    /// Focus before opening the controls; the outer Option records that they took it.
    return_focus: Option<Option<egui::Id>>,
    /// A workflow input that may keep its keyboard focus inside the controls.
    input_focus: Option<egui::Id>,
    pub selected: Option<(RepoId, u64)>,
    pub methods: BTreeMap<RepoId, MergeMethod>,
    pub confirm: Option<Confirm>,
    /// Scroll the selected row into view on the next frame.
    pub scroll_to_selected: bool,
    /// The host whose chip was clicked: "Disconnect GitHub?" shows until answered.
    pub disconnect: Option<String>,
    /// Per repository, whether its Other group was opened or closed by hand.
    pub others: BTreeMap<RepoId, bool>,
}

/// Other people's pull requests start folded when the viewer has their own or assigned ones to
/// look at and there are more than this many others.
const FOLD_OTHERS: usize = 8;

impl GithubUi {
    /// Whether `repo`'s Other group shows its rows.
    fn others_open(&self, repo: &RepoId, rows: &[Row]) -> bool {
        if let Some(open) = self.others.get(repo) {
            return *open;
        }
        let of_repo = || rows.iter().filter(|row| row.repo == *repo);
        let others = of_repo().filter(|row| row.group == Group::Other).count();
        others <= FOLD_OTHERS || of_repo().all(|row| row.group == Group::Other)
    }
}

/// The action `action` on the pull request `pull` of `repo`, ready to confirm: its ref and
/// inputs filled in, and the inputs left empty to ask for.
pub(super) fn dispatch_confirm(repo: &RepoId, action: &Action, pull: Option<&Pull>) -> Result<Confirm, String> {
    let context = pull.map(|pull| PrContext {
        number: pull.number,
        head_sha: &pull.head_sha,
        head: &pull.head,
        base: &pull.base,
    });
    let name = repo.full_name();
    let git_ref = action
        .ref_template()
        .map(|template| atomics_core::github::fill(template, &name, context.as_ref()))
        .transpose()?;
    let mut inputs = BTreeMap::new();
    let mut asked = Vec::new();
    for (key, template) in &action.inputs {
        let value = atomics_core::github::fill(template, &name, context.as_ref())?;
        if value.is_empty() {
            asked.push(key.clone());
        }
        inputs.insert(key.clone(), value);
    }
    Ok(Confirm::Dispatch { repo: repo.clone(), action: action.clone(), git_ref, inputs, asked })
}

/// What "editor" says on a pull request diff, whose files are in no worktree here.
pub(super) const READ_ONLY: &str = "A pull request diff is read-only; open the pull request to edit its branch";

/// A commit's short form, as GitHub shows it.
pub(super) fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// GitHub's page for revoking an app's authorization; ATOMICS never calls the revoke API, which
/// needs the app's secret.
pub(super) fn revoke_url(host: &str) -> String {
    format!("https://{host}/settings/apps/authorizations")
}

/// The GitHub shortcut ATOMICS takes, and what is wrong with the configured one. A binding that
/// does not parse, or that another `[keys]` binding, Ctrl/Command+1…9 or Alt+1…9 already uses, falls back to
/// `primary+g`, unless that is taken too; then there is no shortcut and the top-bar icon remains.
pub(super) fn github_shortcut(
    keys: &atomics_core::config::KeyConfig,
) -> (Option<egui::KeyboardShortcut>, Option<String>) {
    if keys.github.trim().is_empty() {
        return (None, None);
    }
    let taken = atomics_ui::keys::taken_shortcuts(
        atomics_ui::keys::bindings(keys),
        "github",
        egui::os::OperatingSystem::from_target_os(),
    );
    let owner = |chord: &egui::KeyboardShortcut| atomics_ui::keys::shortcut_owner(&taken, chord).map(str::to_string);
    let problem = match shortcut(&keys.github) {
        None => format!("[keys] github = {:?} is not a shortcut", keys.github),
        Some(chord) => match owner(&chord) {
            None => return (Some(chord), None),
            Some(name) => format!("[keys] github = {:?} is already {name}", keys.github),
        },
    };
    let fallback = shortcut("primary+g").filter(|chord| owner(chord).is_none());
    let using = if fallback.is_some() { "using primary+g" } else { "no GitHub shortcut" };
    (fallback, Some(format!("{problem}; {using}")))
}

/// What the config asks for that cannot work, as one top-bar line; the rest of the config applies.
pub(super) fn config_problems(config: &Config) -> Option<String> {
    let mut problems = config.github.problems();
    problems.extend(
        atomics_ui::keys::invalid_bindings(&config.keys, egui::os::OperatingSystem::from_target_os())
            .into_iter()
            .filter(|name| *name != "github")
            .map(|name| format!("[keys] {name} is not a shortcut")),
    );
    problems.extend(github_shortcut(&config.keys).1);
    (!problems.is_empty()).then(|| problems.join("; "))
}

/// Paints a small git branch: two commits on a line and a branch off it.
fn paint_git_icon(painter: &egui::Painter, center: Pos2, color: Color32) {
    let stroke = Stroke::new(1.4, color);
    let (top, bottom, side) = (center + vec2(-3.5, -5.0), center + vec2(-3.5, 5.0), center + vec2(4.0, -2.5));
    painter.line_segment([top, bottom], stroke);
    painter.add(egui::Shape::QuadraticBezier(egui::epaint::QuadraticBezierShape::from_points_stroke(
        [side, side + vec2(0.0, 5.5), center + vec2(-3.5, 3.0)],
        false,
        Color32::TRANSPARENT,
        stroke,
    )));
    for point in [top, bottom, side] {
        painter.circle(point, 2.0, color, Stroke::NONE);
    }
}

fn github_focus_id() -> egui::Id {
    egui::Id::unique("github-controls-focus")
}

impl Desktop<'_> {
    /// Take focus as soon as the controls open, before the terminal or editor sees input.
    pub(super) fn github_focus(&mut self, ctx: &egui::Context) {
        if self.runtime.github.modal {
            if self.client.github.return_focus.is_none() {
                self.client.github.return_focus = Some(ctx.memory(|memory| memory.focused()));
                self.client.focus_terminal = None;
                self.client.ui.focus_editor = false;
                self.client.focus_steer = false;
            }
            if !self.client.github.input_focus.is_some_and(|id| ctx.memory(|memory| memory.has_focus(id))) {
                ctx.memory_mut(|memory| memory.request_focus(github_focus_id()));
                ctx.memory_mut(|memory| {
                    memory.set_focus_lock_filter(
                        github_focus_id(),
                        egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
                    );
                });
            }
        } else if let Some(previous) = self.client.github.return_focus.take() {
            ctx.memory_mut(|memory| {
                if let Some(focused) = memory.focused() {
                    memory.surrender_focus(focused);
                }
                if let Some(previous) = previous {
                    memory.request_focus(previous);
                }
            });
        }
    }

    /// The top-bar counter: a git icon and the open pull requests the filter counts.
    pub(super) fn github_counter(&mut self, ui: &mut egui::Ui, in_menu: bool) {
        let counter = self.runtime.github.counter(&self.runtime.config.github);
        let (label, color, hover) = match &counter {
            Counter::Hidden => return,
            Counter::SignedOut => {
                let hover = if self.runtime.github.needs_connect() {
                    "GitHub: connect to see pull requests"
                } else {
                    "Azure DevOps: run az login to see pull requests"
                };
                ("–".to_string(), self.palette().muted, hover.to_string())
            }
            Counter::Count { count, failing } => (
                count.to_string(),
                if *failing { self.palette().warning } else { self.palette().foreground },
                format!(
                    "{count} pull request{} {}{}",
                    if *count == 1 { "" } else { "s" },
                    self.runtime.config.github.filter.label(),
                    if *failing { " · some checks are failing" } else { "" }
                ),
            ),
        };
        let response = ui.add(egui::Button::new(RichText::new(format!("     {label}")).color(color)));
        if !ui.is_sizing_pass() {
            paint_git_icon(ui.painter(), pos2(response.rect.left() + 13.0, response.rect.center().y), color);
        }
        super::topbar::record_item(ui, "github", &response);
        let hover = match github_shortcut(&self.runtime.config.keys).0 {
            Some(chord) => format!("{hover} ({})", ui.ctx().format_shortcut(&chord)),
            None => hover,
        };
        if response.on_hover_text(hover).clicked() {
            let open = !self.runtime.github.modal;
            self.runtime.github_set_modal(open);
            self.github_focus(ui.ctx());
            if in_menu {
                ui.close();
            }
        }
    }

    pub(super) fn github_modal(&mut self, ctx: &egui::Context) {
        self.github_focus(ctx);
        self.github_confirm_dialog(ctx);
        if !self.runtime.github.modal {
            return;
        }
        let p = self.palette();
        let mut open = true;
        let title = if self.runtime.github.repos().iter().any(RepoId::is_azure) { "Pull requests" } else { "GitHub" };
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.interact(ui.max_rect(), github_focus_id(), Sense::focusable_noninteractive());
                ui.set_width(560.0);
                self.github_header(ui, &p);
                if !self.runtime.github.repos().is_empty() && self.runtime.github.signed_in_hosts().is_empty() {
                    return;
                }
                ui.separator();
                self.github_installs(ui);
                self.github_pulls(ui, &p);
                self.github_selected_actions(ui, &p);
                self.github_repo_actions(ui, &p);
                if let Some(notice) = self.runtime.github.notice.clone() {
                    ui.separator();
                    ui.label(RichText::new(notice).color(p.muted));
                }
            });
        if !open {
            self.runtime.github_set_modal(false);
        }
        // A new window's invisible sizing pass may discard its widget's focus registration.
        self.github_focus(ctx);
    }

    fn github_header(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let mut connect = false;
        let mut use_gh = None;
        let mut cancel = false;
        let mut reopen = None;
        ui.horizontal(|ui| {
            for (host, login) in self.runtime.github.signed_in_hosts() {
                if host == AZURE_HOST {
                    let chip = RichText::new(format!("Azure DevOps · {login}")).color(p.on_accent);
                    ui.add(egui::Button::new(chip).fill(p.accent).sense(Sense::hover()))
                        .on_hover_text("Signed in with the Azure CLI; az logout signs out");
                    continue;
                }
                let chip = if host == atomics_core::github::DEFAULT_HOST {
                    format!("GitHub · {login}")
                } else {
                    format!("GitHub {host} · {login}")
                };
                let response = ui.add(egui::Button::new(RichText::new(chip).color(p.on_accent)).fill(p.accent));
                if response.on_hover_text("Signed in; the token stays in the OS keychain").clicked() {
                    self.client.github.disconnect = Some(host);
                }
            }
            if let Some(error) = self.runtime.github.error() {
                ui.label(RichText::new(error).color(p.warning).size(11.0));
            }
        });
        if let Some(host) = self.runtime.github.revoke.clone() {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Signed out; the token was deleted from the keychain.").color(p.muted));
                let link = ui.add(
                    egui::Label::new(RichText::new("Revoke on GitHub").color(p.accent).underline())
                        .sense(Sense::click()),
                );
                if link.on_hover_text("Remove ATOMICS's authorization on GitHub as well").clicked() {
                    self.runtime.github_open_url(revoke_url(&host));
                }
            });
        }
        if let Some(host) = self.client.github.disconnect.clone() {
            ui.horizontal(|ui| {
                ui.label(format!("Disconnect GitHub on {host}? The stored token is removed."));
                if ui.button("Disconnect").clicked() {
                    self.runtime.github_disconnect(host.clone());
                    self.client.github.disconnect = None;
                }
                if ui.button("Keep").clicked() {
                    self.client.github.disconnect = None;
                }
            });
        }
        let needs = self.runtime.github.needs_connect();
        match &self.runtime.github.connect {
            Connect::Idle if needs => {
                ui.label("Connect GitHub to see, review and merge pull requests here.");
                connect = ui.button("Connect GitHub").clicked();
            }
            Connect::Idle => {}
            Connect::Checking => {
                ui.label(RichText::new("Connecting to GitHub…").color(p.muted));
            }
            Connect::ConfirmGh { login, .. } => {
                ui.label(format!("Use your gh account {login}?"));
                ui.horizontal(|ui| {
                    if ui.button(format!("Use {login}")).clicked() {
                        use_gh = Some(true);
                    }
                    if ui.button("Sign in another way").clicked() {
                        use_gh = Some(false);
                    }
                });
            }
            Connect::Device { user_code, uri, copied, .. } => {
                if !copied {
                    ui.ctx().copy_text(user_code.clone());
                }
                ui.label(RichText::new(user_code).monospace().size(22.0).strong().color(p.accent));
                ui.label("The code is on your clipboard: paste it in the browser and press Authorize.");
                ui.horizontal(|ui| {
                    if ui.button("Open the page again").clicked() {
                        reopen = Some(uri.clone());
                    }
                    cancel = ui.button("Cancel").clicked();
                });
            }
            Connect::Failed(error) => {
                ui.label(RichText::new(error).color(p.error));
                connect = ui.button("Connect GitHub").clicked();
            }
        }
        if self.runtime.github.needs_az_login() {
            ui.label("Azure DevOps uses the Azure CLI's sign-in: run az login in a terminal, then check again.");
            if ui.button("Check again").clicked() {
                self.runtime.github_poll_now();
            }
        }
        if let Connect::Device { copied, .. } = &mut self.runtime.github.connect {
            *copied = true;
        }
        if connect {
            self.runtime.github_connect();
        }
        if let Some(accept) = use_gh {
            self.runtime.github_use_gh(accept);
        }
        if cancel {
            self.runtime.github_cancel_connect();
        }
        if let Some(url) = reopen {
            self.runtime.github_open_url(url);
        }
    }

    /// While the controls are open they hold the keyboard, so nothing typed reaches the terminal or
    /// editor that had focus. ↓/↑ (also with Shift) select the next or previous pull request, PageDown/PageUp
    /// jump to the next or previous repository's, Enter opens it, D loads its diff, M asks to merge
    /// it and Esc closes. In a confirmation, Enter confirms and Esc cancels. Letters match the key
    /// the layout types. All other keys and text are dropped, except the application chords and
    /// Ctrl/Command+1…9, which still work. A text field (a workflow input) keeps its keys.
    pub(super) fn github_keys(&mut self, ctx: &egui::Context) {
        if !self.runtime.github.modal
            || self.client.github.input_focus.is_some_and(|id| ctx.memory(|memory| memory.has_focus(id)))
        {
            return;
        }
        #[derive(Clone, Copy, PartialEq)]
        enum Move {
            Row(i32),
            Section(i32),
            Open,
            Diff,
            Merge,
            Close,
            Confirm,
            Cancel,
        }
        let confirming = self.client.github.confirm.is_some();
        let agent = if cfg!(target_os = "macos") { egui::Modifiers::MAC_CMD } else { egui::Modifiers::CTRL };
        let found = ctx.input_mut(|input| {
            let mut found = None;
            input.events.retain(|event| {
                let (key, physical, repeat, modifiers) = match event {
                    // Typed or pasted text would land in the terminal behind the controls.
                    egui::Event::Text(_) | egui::Event::Paste(_) => return false,
                    egui::Event::Key { key, physical_key, pressed: true, repeat, modifiers } => {
                        (*key, physical_key.unwrap_or(*key), *repeat, *modifiers)
                    }
                    _ => return true,
                };
                if crate::theme::is_app_chord(&modifiers)
                    || (crate::theme::is_digit(physical) && modifiers.matches_exact(agent))
                {
                    return true;
                }
                if found.is_none() {
                    let shift = modifiers.matches_exact(egui::Modifiers::SHIFT);
                    let once = modifiers.is_none() && !repeat;
                    found = match key {
                        Key::Escape if once => Some(if confirming { Move::Cancel } else { Move::Close }),
                        Key::Enter if once => Some(if confirming { Move::Confirm } else { Move::Open }),
                        _ if confirming => None,
                        Key::ArrowDown if shift || modifiers.is_none() => Some(Move::Row(1)),
                        Key::ArrowUp if shift || modifiers.is_none() => Some(Move::Row(-1)),
                        Key::PageDown if modifiers.is_none() => Some(Move::Section(1)),
                        Key::PageUp if modifiers.is_none() => Some(Move::Section(-1)),
                        Key::D if once => Some(Move::Diff),
                        Key::M if once => Some(Move::Merge),
                        _ => None,
                    };
                }
                false
            });
            found
        });
        let Some(found) = found else { return };
        match found {
            Move::Close => {
                self.runtime.github_set_modal(false);
                self.github_focus(ctx);
                return;
            }
            Move::Cancel => {
                self.client.github.confirm = None;
                return;
            }
            Move::Confirm => {
                if let Some(confirm) = self.client.github.confirm.take() {
                    self.github_run_confirmed(confirm);
                }
                return;
            }
            _ => {}
        }
        let (rows, selected) = self.github_rows();
        let Some(current) = selected else { return };
        let next = match found {
            Move::Row(step) => (current as i32 + step).clamp(0, rows.len() as i32 - 1) as usize,
            Move::Section(1) => rows
                .iter()
                .skip(current)
                .position(|row| row.repo != rows[current].repo)
                .map_or(current, |i| current + i),
            Move::Section(_) => {
                // The first row of this repository, or of the one before when already there.
                let start = |i: usize| (0..=i).rev().take_while(|&j| rows[j].repo == rows[i].repo).last().unwrap_or(i);
                let own = start(current);
                if own == current && current > 0 { start(current - 1) } else { own }
            }
            _ => current,
        };
        let row = rows[next].clone();
        self.client.github.selected = Some((row.repo.clone(), row.pull.number));
        self.client.github.scroll_to_selected = true;
        match found {
            Move::Open => self.runtime.github_open_url(row.pull.url.clone()),
            Move::Diff => self.runtime.github_view_diff(row.repo.clone(), &row.pull),
            Move::Merge => {
                let mergeable = row.details.as_ref().is_some_and(|details| details.buttons(row.pull.draft).merge);
                let methods = self.runtime.github.methods(&row.repo);
                let method = self.client.github.methods.get(&row.repo).copied().filter(|m| methods.contains(m));
                match method.or_else(|| methods.first().copied()) {
                    Some(method) if mergeable => {
                        self.client.github.confirm = Some(self.runtime.github.merge_confirm(&row, method, false))
                    }
                    _ => {
                        self.runtime.github.notice = Some(format!("{}#{} cannot merge yet", row.repo, row.pull.number))
                    }
                }
            }
            _ => {}
        }
    }

    /// "Install ATOMICS on <owner>" for each account a listed repository is on and the GitHub App is not.
    fn github_installs(&mut self, ui: &mut egui::Ui) {
        let slug = self.runtime.config.github.app_slug().to_string();
        for (owner, url) in self.runtime.github.installs(&slug) {
            let response = ui.button(format!("Install ATOMICS on {owner}"));
            if response
                .on_hover_text("The GitHub App is not installed there, so its pull requests are out of reach")
                .clicked()
            {
                self.runtime.github_open_url(url);
            }
        }
    }

    /// The rows the modal lists, in order, and the selected one (the first by default).
    pub(super) fn github_rows(&mut self) -> (Vec<Row>, Option<usize>) {
        let all = self.runtime.github.rows(self.runtime.config.github.filter);
        let ui = &self.client.github;
        let folded: BTreeSet<&RepoId> =
            all.iter().map(|row| &row.repo).filter(|repo| !ui.others_open(repo, &all)).collect();
        let rows: Vec<Row> =
            all.iter().filter(|row| row.group != Group::Other || !folded.contains(&row.repo)).cloned().collect();
        let index = self
            .client
            .github
            .selected
            .as_ref()
            .and_then(|(repo, number)| rows.iter().position(|row| &row.repo == repo && row.pull.number == *number));
        let index = index.or((!rows.is_empty()).then_some(0));
        self.client.github.selected = index.map(|i| (rows[i].repo.clone(), rows[i].pull.number));
        (rows, index)
    }

    fn github_pulls(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let (rows, selected) = self.github_rows();
        let scroll = std::mem::take(&mut self.client.github.scroll_to_selected);
        egui::ScrollArea::vertical().id_salt("github-pulls").max_height(320.0).show(ui, |ui| {
            for repo in self.runtime.github.repos().to_vec() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(repo.to_string()).strong());
                    if let Some(error) = self.runtime.github.repo_error(&repo) {
                        ui.label(RichText::new(error).color(p.warning).size(11.0));
                    }
                });
                let mine: Vec<(usize, &Row)> = rows.iter().enumerate().filter(|(_, row)| row.repo == repo).collect();
                let all = self.runtime.github.rows(self.runtime.config.github.filter);
                let others = all.iter().filter(|row| row.repo == repo && row.group == Group::Other).count();
                let grouped = all.iter().any(|row| row.repo == repo && row.group != Group::Other);
                if mine.is_empty() && others == 0 {
                    ui.label(RichText::new("No open pull requests").color(p.muted).size(11.0));
                }
                // Headings only when the viewer has pull requests of their own or assigned here; the
                // last pass (`None`) heads a folded Other group, whose rows are not listed.
                let mut heading = None;
                for row in mine.into_iter().map(Some).chain([None]) {
                    let group = row.map_or(Group::Other, |(_, row)| row.group);
                    if grouped && heading != Some(group) && (group != Group::Other || others > 0) {
                        heading = Some(group);
                        if group == Group::Other {
                            let open = self.client.github.others_open(&repo, &all);
                            let label = format!("{} {} ({others})", if open { "▾" } else { "▸" }, group.label());
                            let toggle = ui.add(
                                egui::Label::new(RichText::new(label).color(p.muted).size(11.0)).sense(Sense::click()),
                            );
                            if toggle.on_hover_text("Show or hide other people's pull requests").clicked() {
                                self.client.github.others.insert(repo.clone(), !open);
                            }
                        } else {
                            ui.label(RichText::new(group.label()).color(p.muted).size(11.0));
                        }
                    }
                    let Some((i, row)) = row else { break };
                    let pull = &row.pull;
                    let (symbol, color) = match row.details.as_ref().map(|d| d.checks) {
                        Some(CheckState::Passing) => ("✓", p.success),
                        Some(CheckState::Failing) => ("✗", p.error),
                        Some(CheckState::Pending) => ("●", p.warning),
                        _ => ("·", p.muted),
                    };
                    let status = match &row.details {
                        Some(details) => format!(
                            "{} · {} · {}",
                            pull.author,
                            details.review_label(),
                            details.merge_state.to_lowercase().replace('_', " ")
                        ),
                        None => pull.author.clone(),
                    };
                    let response = ui
                        .horizontal(|ui| {
                            ui.label(RichText::new(symbol).color(color));
                            let title = format!(
                                "#{} {}{}{}",
                                pull.number,
                                if pull.draft { "[draft] " } else { "" },
                                pull.title,
                                if row.counted { " ◆" } else { "" }
                            );
                            let response = ui.selectable_label(selected == Some(i), title);
                            ui.label(RichText::new(status).color(p.muted).size(11.0));
                            response
                        })
                        .inner;
                    if scroll && selected == Some(i) {
                        response.scroll_to_me(Some(egui::Align::Center));
                    }
                    if response.clicked() {
                        self.client.github.selected = Some((row.repo.clone(), pull.number));
                    }
                    if response.double_clicked() {
                        self.runtime.github_open_url(pull.url.clone());
                    }
                }
                ui.add_space(4.0);
            }
        });
    }

    fn github_selected_actions(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let (rows, selected) = self.github_rows();
        let Some(row) = selected.map(|i| rows[i].clone()) else { return };
        ui.separator();
        let (repo, pull) = (row.repo.clone(), row.pull.clone());
        ui.label(RichText::new(format!("{repo}#{} · {} → {}", pull.number, pull.head, pull.base)).color(p.muted));
        if let Some(details) = &row.details {
            let checks: Vec<String> =
                details.check_runs.iter().map(|check| format!("{} {}", check.state.symbol(), check.name)).collect();
            if !checks.is_empty() {
                ui.label(RichText::new(checks.join("  ")).size(11.0).color(p.muted));
            }
        }
        let methods = self.runtime.github.methods(&repo);
        let method = self
            .client
            .github
            .methods
            .get(&repo)
            .copied()
            .filter(|method| methods.contains(method))
            .or_else(|| methods.first().copied());
        let buttons = row.details.as_ref().map(|details| details.buttons(pull.draft));
        let busy = self.runtime.github.is_busy(&repo, pull.number);
        let mut confirm = None;
        ui.horizontal(|ui| {
            if ui.button("Open").on_hover_text("Open the pull request in the browser").clicked() {
                self.runtime.github_open_url(pull.url.clone());
            }
            if ui.button("View diff").on_hover_text("Review its diff in the diff viewer, read-only").clicked() {
                self.runtime.github_view_diff(repo.clone(), &pull);
            }
            if let Some(method) = method {
                let merge = ui
                    .add_enabled(buttons.is_some_and(|b| b.merge) && !busy, egui::Button::new("Merge"))
                    .on_hover_text(method.label())
                    .on_disabled_hover_text(match &row.details {
                        Some(details) if pull.draft => format!("Draft · {}", details.merge_state),
                        Some(details) => format!("Not mergeable yet: {}", details.merge_state),
                        None => "Loading checks…".into(),
                    });
                if merge.clicked() {
                    confirm = Some((method, false));
                }
                if methods.len() > 1 {
                    egui::ComboBox::from_id_salt(("github-method", &repo)).selected_text(method.api_name()).show_ui(
                        ui,
                        |ui| {
                            for option in &methods {
                                if ui.selectable_label(*option == method, option.label()).clicked() {
                                    self.client.github.methods.insert(repo.clone(), *option);
                                }
                            }
                        },
                    );
                }
                if buttons.is_some_and(|b| b.admin)
                    && ui
                        .add_enabled(!busy, egui::Button::new(RichText::new("Admin merge").color(p.warning)))
                        .on_hover_text("Merge past the branch rules; you may bypass them")
                        .clicked()
                {
                    confirm = Some((method, true));
                }
            }
            if busy {
                ui.label(RichText::new("merging…").color(p.muted));
            }
        });
        if let Some((method, admin)) = confirm {
            self.client.github.confirm = Some(self.runtime.github.merge_confirm(&row, method, admin));
        }
        let actions: Vec<Action> = (self.runtime.config.github.actions.iter())
            .filter(|action| action.scope == Scope::Pr && action.applies_to(&repo))
            .cloned()
            .collect();
        if !actions.is_empty() {
            ui.horizontal_wrapped(|ui| {
                for action in actions {
                    self.github_action_button(ui, p, &repo, &action, Some(&pull));
                }
            });
        }
    }

    fn github_repo_actions(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let mut shown = false;
        for repo in self.runtime.github.repos().to_vec() {
            let actions: Vec<Action> = (self.runtime.config.github.actions.iter())
                .filter(|action| action.scope == Scope::Repo && action.applies_to(&repo))
                .cloned()
                .collect();
            if actions.is_empty() || self.runtime.github.viewer(&repo.host).is_none() {
                continue;
            }
            if !shown {
                ui.separator();
                shown = true;
            }
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(repo.to_string()).color(p.muted));
                for action in actions {
                    self.github_action_button(ui, p, &repo, &action, None);
                }
            });
        }
    }

    fn github_action_button(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        repo: &RepoId,
        action: &Action,
        pull: Option<&Pull>,
    ) {
        let busy = self.runtime.github.is_dispatching(repo, &action.label);
        let response = ui.add_enabled(!busy, egui::Button::new(&action.label));
        let runner = if repo.is_azure() { "pipeline" } else { "GitHub Actions workflow" };
        let response = response.on_hover_text(format!("Run {runner} {}", action.workflow));
        if response.clicked() {
            match dispatch_confirm(repo, action, pull) {
                Ok(confirm) => self.client.github.confirm = Some(confirm),
                Err(error) => self.runtime.github.notice = Some(format!("{}: {error}", action.label)),
            }
        }
        match self.runtime.github.run(repo, &action.label) {
            Some(Ok(Some(run))) => {
                let color = match run.state() {
                    CheckState::Passing => p.success,
                    CheckState::Failing => p.error,
                    CheckState::Pending => p.warning,
                    CheckState::None => p.muted,
                };
                let link =
                    ui.add(egui::Label::new(RichText::new(run.label()).color(color).size(11.0)).sense(Sense::click()));
                if link.on_hover_text("Latest run; click to open it").clicked() {
                    self.runtime.github_open_url(run.url.clone());
                }
            }
            Some(Ok(None)) => {
                ui.label(RichText::new("never run").color(p.muted).size(11.0));
            }
            Some(Err(error)) => {
                ui.label(RichText::new("run status unavailable").color(p.muted).size(11.0)).on_hover_text(error);
            }
            None => {}
        }
    }

    /// Merges, admin merges and dispatches ask once, like closing a session.
    fn github_confirm_dialog(&mut self, ctx: &egui::Context) {
        self.client.github.input_focus = None;
        let Some(mut confirm) = self.client.github.confirm.take() else { return };
        // Closing the controls drops a confirmation they asked for.
        if !self.runtime.github.modal {
            return;
        }
        let mut keep = true;
        let mut go = false;
        let title = match &confirm {
            Confirm::Merge { admin: true, .. } => "Admin merge pull request?",
            Confirm::Merge { .. } => "Merge pull request?",
            Confirm::Dispatch { .. } => "Run workflow?",
        };
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                let label = match &mut confirm {
                    Confirm::Merge { repo, number, title, method, sha, latest, reviewed, admin } => {
                        ui.label(format!("{repo}#{number} “{title}”"));
                        ui.label(method.label());
                        ui.label(if *reviewed {
                            format!("Head {}, the commit whose diff you loaded", short_sha(sha))
                        } else {
                            format!("Head {}; its diff was not loaded here", short_sha(sha))
                        });
                        if sha != latest {
                            ui.label(
                                RichText::new(format!(
                                    "The branch has moved to {} since, so GitHub will refuse this merge. \
                                     Load the diff again to review the new head.",
                                    short_sha(latest)
                                ))
                                .color(self.palette().warning),
                            );
                        }
                        if *admin {
                            ui.label(
                                RichText::new("This bypasses the branch rules that block it.")
                                    .color(self.palette().warning),
                            );
                        }
                        if *admin { "Admin merge" } else { "Merge" }
                    }
                    Confirm::Dispatch { repo, action, git_ref, inputs, asked } => {
                        ui.label(format!("{} ({}) on {repo}", action.label, action.workflow));
                        ui.label(format!("ref: {}", git_ref.as_deref().unwrap_or("the default branch")));
                        egui::Grid::new("github-inputs").num_columns(2).show(ui, |ui| {
                            for (key, value) in inputs.iter_mut() {
                                ui.label(key.as_str());
                                if asked.contains(key) {
                                    let response = ui.text_edit_singleline(value);
                                    if response.has_focus() {
                                        self.client.github.input_focus = Some(response.id);
                                    }
                                } else {
                                    ui.label(RichText::new(value.as_str()).monospace());
                                }
                                ui.end_row();
                            }
                        });
                        "Run"
                    }
                };
                ui.horizontal(|ui| {
                    go = ui.button(label).clicked();
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        keep = false;
                    }
                });
            });
        if go {
            self.github_run_confirmed(confirm);
        } else if keep {
            self.client.github.confirm = Some(confirm);
        }
    }

    /// Runs what the user just confirmed.
    fn github_run_confirmed(&mut self, confirm: Confirm) {
        match confirm {
            Confirm::Merge { repo, number, method, sha, admin, .. } => {
                self.runtime.github_merge(repo, number, method, sha, admin)
            }
            Confirm::Dispatch { repo, action, git_ref, inputs, .. } => {
                self.runtime.github_dispatch(repo, action, git_ref, inputs)
            }
        }
    }
}

pub(super) fn pr_key(repo: &RepoId, number: u64) -> String {
    format!("pull:{repo}#{number}")
}

impl State {
    pub(super) fn pull_request_infos(&self) -> Vec<atomics_core::projection::PullRequestInfo> {
        let mut rows = BTreeMap::new();
        for (repo, listing) in &self.github.lists {
            for pull in &listing.pulls {
                let key = pr_key(repo, pull.number);
                rows.insert(
                    key.clone(),
                    atomics_core::projection::PullRequestInfo {
                        group: self.github.viewer(&repo.host).map(|viewer| Group::of(pull, viewer)),
                        author: Some(pull.author.clone()),
                        draft: Some(pull.draft),
                        key,
                        repo: repo.to_string(),
                        number: pull.number,
                        title: pull.title.clone(),
                        head: pull.head.clone(),
                        base: pull.base.clone(),
                        url: pull.url.clone(),
                        head_sha: None,
                        loading: false,
                        error: None,
                    },
                );
            }
        }
        for (key, view) in &self.github.pr_diffs {
            if let Some(row) = rows.get_mut(key) {
                row.head_sha = view.head_sha.clone();
                row.loading = view.prepared.is_none();
                row.error = view.prepared.as_ref().and_then(|p| p.as_ref().err().cloned());
                continue;
            }
            let url = String::new();
            rows.insert(
                key.clone(),
                atomics_core::projection::PullRequestInfo {
                    group: None,
                    author: None,
                    draft: None,
                    key: key.clone(),
                    repo: view.repo.to_string(),
                    number: view.number,
                    title: view.title.clone(),
                    head: view.head.clone(),
                    base: view.base.clone(),
                    url,
                    head_sha: view.head_sha.clone(),
                    loading: view.prepared.is_none(),
                    error: view.prepared.as_ref().and_then(|p| p.as_ref().err().cloned()),
                },
            );
        }
        rows.into_values().collect()
    }

    pub(super) fn load_pull_request(&mut self, key: &str) -> bool {
        let found = self
            .github
            .lists
            .iter()
            .find_map(|(repo, list)| {
                list.pulls.iter().find(|p| pr_key(repo, p.number) == key).map(|p| (repo.clone(), p.clone()))
            })
            .or_else(|| {
                self.github.pr_diffs.get(key).map(|v| {
                    (
                        v.repo.clone(),
                        Pull {
                            number: v.number,
                            title: v.title.clone(),
                            head: v.head.clone(),
                            base: v.base.clone(),
                            ..Pull::default()
                        },
                    )
                })
            });
        let Some((repo, pull)) = found else { return false };
        self.fetch_pull_request(repo, &pull, false);
        true
    }

    pub(super) fn pull_request_bodies(&self) -> impl Iterator<Item = (String, u64, Arc<diff_view::Prepared>)> + '_ {
        self.github.pr_diffs.iter().filter_map(|(key, view)| {
            Some((key.clone(), view.revision, view.prepared.as_ref()?.as_ref().ok()?.clone()))
        })
    }
}

#[cfg(test)]
#[path = "github_tests.rs"]
mod tests;
