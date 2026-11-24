use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use atomics_core::usage::{Account, Usage};
use serde_json::{Value, json};
use zeroize::Zeroizing;

const POLL_INTERVAL: Duration = Duration::from_secs(60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_USAGE_BODY: u64 = 256 * 1024;
const MAX_PROBE_BYTES: usize = 128 * 1024;
const TAIL_BYTES: u64 = 16 * 1024;

const POLL_COPILOT: u8 = 1 << 0;
const POLL_CURSOR: u8 = 1 << 1;

pub enum Report {
    Claude(Usage),
    Codex(Usage),
    CodexChief(Usage),
    Copilot(Usage),
    Cursor(Usage),
}

enum PollSignal {
    Wake,
    Stop,
}

pub struct UsagePoller {
    pub(crate) claude: Usage,
    pub(crate) codex: Usage,
    pub(crate) codex_chief: Usage,
    pub(crate) copilot: Usage,
    pub(crate) cursor: Usage,
    reports: Receiver<Report>,
    active: Arc<AtomicU8>,
    control: Sender<PollSignal>,
}

impl UsagePoller {
    pub fn new(window: crate::host::Window) -> (Self, Sender<Report>) {
        let (tx, reports) = channel();
        let (control, signals) = channel();
        let active = Arc::new(AtomicU8::new(0));
        let home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| atomics_core::paths::home().join(".codex"));
        let poll_tx = tx.clone();
        let poll_active = active.clone();
        std::thread::Builder::new()
            .name("atomics-usage".into())
            .spawn(move || {
                let chief = crate::setup::chief_codex_home().join("sessions");
                poll_worker(&home.join("sessions"), Some(&chief), poll_active, poll_tx, signals, window, POLL_INTERVAL);
            })
            .expect("usage worker");
        (
            Self {
                claude: Usage::default(),
                codex: Usage::default(),
                codex_chief: Usage::default(),
                copilot: Usage::default(),
                cursor: Usage::default(),
                reports,
                active,
                control,
            },
            tx,
        )
    }

    pub fn for_provider(&self, provider: &str) -> Option<&Usage> {
        match provider {
            "claude" => Some(&self.claude),
            "codex" => Some(&self.codex),
            "copilot" => Some(&self.copilot),
            "cursor" => Some(&self.cursor),
            _ => None,
        }
    }

    /// Tells the background worker which budget-API providers are currently in use.
    /// Newly activated providers wake the worker immediately instead of waiting for the 60 s tick.
    pub fn set_active_providers(&self, providers: &[String]) {
        let mut mask = 0u8;
        for provider in providers {
            match provider.as_str() {
                "copilot" => mask |= POLL_COPILOT,
                "cursor" => mask |= POLL_CURSOR,
                _ => {}
            }
        }
        let prev = self.active.swap(mask, Ordering::AcqRel);
        if (mask & !prev) != 0 {
            let _ = self.control.send(PollSignal::Wake);
        }
    }

    pub fn drain(&mut self) -> bool {
        let mut changed = false;
        while let Ok(report) = self.reports.try_recv() {
            changed |= match report {
                Report::Claude(usage) => self.claude.update(usage),
                Report::Codex(usage) => self.codex.update(usage),
                Report::CodexChief(usage) => self.codex_chief.update(usage),
                Report::Copilot(usage) => self.copilot.update(usage),
                Report::Cursor(usage) => self.cursor.update(usage),
            };
        }
        changed
    }
}

impl Drop for UsagePoller {
    fn drop(&mut self) {
        let _ = self.control.send(PollSignal::Stop);
    }
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64
}

fn poll_worker(
    sessions: &Path,
    chief: Option<&Path>,
    active: Arc<AtomicU8>,
    tx: Sender<Report>,
    signals: Receiver<PollSignal>,
    window: crate::host::Window,
    interval: Duration,
) {
    let mut codex_sources =
        [(Some(sessions), Report::Codex as fn(Usage) -> Report, None), (chief, Report::CodexChief, None)];
    let mut last_copilot: Option<(Instant, Option<Usage>)> = None;
    let mut last_cursor: Option<(Instant, Option<Usage>)> = None;
    loop {
        for (path, report, previous) in &mut codex_sources {
            if let Some(usage) = path.and_then(read_codex)
                && previous.as_ref() != Some(&usage)
            {
                *previous = Some(usage.clone());
                if tx.send(report(usage)).is_err() {
                    return;
                }
                window.request_repaint();
            }
        }
        let mask = active.load(Ordering::Acquire);
        if mask & POLL_COPILOT != 0 && last_copilot.as_ref().is_none_or(|(at, _)| at.elapsed() >= interval) {
            let prev = last_copilot.as_ref().and_then(|(_, usage)| usage.clone());
            let usage = read_copilot();
            let changed = usage.as_ref().is_some_and(|next| prev.as_ref() != Some(next));
            last_copilot = Some((Instant::now(), usage.clone().or(prev)));
            if changed && let Some(usage) = usage {
                if tx.send(Report::Copilot(usage)).is_err() {
                    return;
                }
                window.request_repaint();
            }
        }
        if mask & POLL_CURSOR != 0 && last_cursor.as_ref().is_none_or(|(at, _)| at.elapsed() >= interval) {
            let prev = last_cursor.as_ref().and_then(|(_, usage)| usage.clone());
            let usage = read_cursor();
            let changed = usage.as_ref().is_some_and(|next| prev.as_ref() != Some(next));
            last_cursor = Some((Instant::now(), usage.clone().or(prev)));
            if changed && let Some(usage) = usage {
                if tx.send(Report::Cursor(usage)).is_err() {
                    return;
                }
                window.request_repaint();
            }
        }
        match signals.recv_timeout(interval) {
            Ok(PollSignal::Wake) | Err(RecvTimeoutError::Timeout) => {}
            Ok(PollSignal::Stop) | Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn usage_http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(HTTP_TIMEOUT))
        .http_status_as_error(false)
        .https_only(true)
        .max_redirects(0)
        .user_agent(concat!("atomics/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// Runs a CLI or secret-store helper off the UI thread with a strict timeout.
/// Output stays in memory in a [`Zeroizing`] buffer and never touches disk.
fn run_bounded_stdout(program: &Path, args: &[&str], timeout: Duration) -> Option<Zeroizing<String>> {
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    for name in ["ATOMICS_SOCKET", "ATOMICS_PANE", "ATOMICS_HOME", "ATOMICS_CONFIG", atomics_core::hook::HOOK_PANE_ENV]
    {
        command.env_remove(name);
    }
    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Zeroizing::new(Vec::new());
        let _ = stdout.take(MAX_PROBE_BYTES as u64).read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break None,
        }
    };
    let bytes = reader.join().ok()?;
    status?.success().then(|| Zeroizing::new(String::from_utf8_lossy(&bytes).into_owned()))
}

// ---------------------------------------------------------------------------
// GitHub Copilot usage & account (config.json / env token + /copilot_internal/user)
// ---------------------------------------------------------------------------

struct CopilotAuth {
    host: String,
    login: Option<String>,
    token: Option<Zeroizing<String>>,
}

fn copilot_home() -> PathBuf {
    std::env::var_os("COPILOT_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| atomics_core::paths::home().join(".copilot"))
}

fn strip_jsonc_comments(text: &str) -> String {
    text.lines().filter(|line| !line.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n")
}

fn parse_copilot_config(text: &str) -> CopilotAuth {
    let cleaned = strip_jsonc_comments(text);
    let Ok(value) = serde_json::from_str::<Value>(&cleaned) else {
        return CopilotAuth { host: "https://github.com".into(), login: None, token: None };
    };
    let user = value
        .get("lastLoggedInUser")
        .filter(|u| u.is_object())
        .or_else(|| value.get("loggedInUsers").and_then(Value::as_array).and_then(|list| list.first()));
    let host = user
        .and_then(|u| u["host"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("https://github.com")
        .to_owned();
    let login = user.and_then(|u| u["login"].as_str()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let token = login.as_deref().and_then(|login| {
        let key = format!("{host}:{login}");
        value
            .get("copilotTokens")
            .and_then(|tokens| tokens.get(&key))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| Zeroizing::new(s.to_owned()))
    });
    CopilotAuth { host, login, token }
}

fn copilot_user_url(host: &str) -> Option<String> {
    let trimmed = host.trim().trim_end_matches('/');
    let domain = trimmed.strip_prefix("https://")?;
    if domain.is_empty() || domain.chars().any(|c| c.is_whitespace() || matches!(c, '/' | '?' | '#' | '@')) {
        return None;
    }
    let lower = domain.to_ascii_lowercase();
    if lower == "github.com" {
        Some("https://api.github.com/copilot_internal/user".into())
    } else if lower.ends_with(".ghe.com") {
        Some(format!("https://api.{domain}/copilot_internal/user"))
    } else {
        Some(format!("https://{domain}/api/v3/copilot_internal/user"))
    }
}

fn read_copilot() -> Option<Usage> {
    let config_path = copilot_home().join("config.json");
    let mut auth = std::fs::read_to_string(&config_path)
        .ok()
        .map(|text| parse_copilot_config(&text))
        .unwrap_or(CopilotAuth { host: "https://github.com".into(), login: None, token: None });
    // `copilot help environment`: COPILOT_GITHUB_TOKEN > GH_TOKEN > GITHUB_TOKEN.
    let env_token = ["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
        .map(Zeroizing::new);
    if env_token.is_some() {
        auth.token = env_token;
    }
    let observed = now();
    if let Some(token) = &auth.token
        && let Some(url) = copilot_user_url(&auth.host)
        && let Some(payload) = fetch_copilot_user(&url, token)
        && let Some(mut usage) = Usage::copilot_user(&payload, observed)
    {
        if usage.account.as_ref().and_then(|a| a.user.as_ref()).is_none()
            && let Some(login) = auth.login
        {
            let account = usage.account.get_or_insert_with(Account::default);
            account.user = Some(login);
        }
        return Some(usage);
    }
    auth.login.map(|login| Usage {
        observed_at: observed,
        account: Some(Account { user: Some(login), org: None }),
        ..Usage::default()
    })
}

fn fetch_copilot_user(url: &str, token: &Zeroizing<String>) -> Option<Value> {
    let agent = usage_http_agent();
    let header = Zeroizing::new(format!("token {}", token.as_str()));
    let mut response = agent
        .get(url)
        .header("authorization", header.as_str())
        .header("accept", "application/json")
        .header("editor-version", "vscode/1.99.0")
        .header("x-github-api-version", "2025-04-01")
        .call()
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let bytes = response.body_mut().with_config().limit(MAX_USAGE_BODY).read_to_vec().ok()?;
    serde_json::from_slice(&bytes).ok()
}

// ---------------------------------------------------------------------------
// Cursor usage & account (auth.json / OS secret store + DashboardService RPC,
// with `agent status`/`agent about` JSON fallback for account identity)
// ---------------------------------------------------------------------------

fn cursor_api_base(raw: Option<&str>) -> Option<String> {
    let base = raw.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("https://api2.cursor.sh").trim_end_matches('/');
    let rest = base.strip_prefix("https://")?;
    if rest.is_empty() || rest.chars().any(|c| c.is_whitespace() || matches!(c, '?' | '#' | '@')) {
        return None;
    }
    Some(base.to_owned())
}

fn cursor_auth_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let home = atomics_core::paths::home();
    if cfg!(target_os = "macos") {
        paths.push(home.join(".cursor/auth.json"));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        paths.push(PathBuf::from(xdg).join("cursor/auth.json"));
    } else {
        paths.push(home.join(".config/cursor/auth.json"));
    }
    if !cfg!(target_os = "macos") {
        paths.push(home.join(".cursor/auth.json"));
    }
    paths
}

fn parse_cursor_auth_secret(raw: &str) -> Option<Zeroizing<String>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return ["accessToken", "apiKey"]
            .into_iter()
            .find_map(|key| value.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()))
            .map(|token| Zeroizing::new(token.to_owned()));
    }
    Some(Zeroizing::new(trimmed.to_owned()))
}

fn read_cursor_secret_store() -> Option<Zeroizing<String>> {
    let output = if cfg!(target_os = "macos") {
        run_bounded_stdout(
            Path::new("/usr/bin/security"),
            &["find-generic-password", "-s", "cursor-credentials", "-a", "cursor-user", "-w"],
            PROBE_TIMEOUT,
        )?
    } else {
        let program = crate::setup::find_program("secret-tool")?;
        run_bounded_stdout(
            &program,
            &["lookup", "service", "cursor-credentials", "account", "cursor-user"],
            PROBE_TIMEOUT,
        )?
    };
    parse_cursor_auth_secret(&output)
}

fn cursor_token() -> Option<Zeroizing<String>> {
    if let Ok(key) = std::env::var("CURSOR_API_KEY")
        && !key.trim().is_empty()
    {
        return Some(Zeroizing::new(key.trim().to_owned()));
    }
    for path in cursor_auth_paths() {
        let Ok(raw) = std::fs::read_to_string(&path) else { continue };
        let raw = Zeroizing::new(raw);
        let Ok(value) = serde_json::from_str::<Value>(&raw) else { continue };
        if value["storedIn"].as_str() == Some("os-secret-store") {
            return read_cursor_secret_store();
        }
        if let Some(token) = parse_cursor_auth_secret(&raw) {
            return Some(token);
        }
    }
    None
}

fn cursor_rpc(agent: &ureq::Agent, base: &str, method: &str, token: &Zeroizing<String>, body: &Value) -> Option<Value> {
    let url = format!("{base}/aiserver.v1.DashboardService/{method}");
    let auth = Zeroizing::new(format!("Bearer {}", token.as_str()));
    let mut response = agent
        .post(&url)
        .header("authorization", auth.as_str())
        .header("content-type", "application/json")
        .header("connect-protocol-version", "1")
        .send(body.to_string().as_bytes())
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let bytes = response.body_mut().with_config().limit(MAX_USAGE_BODY).read_to_vec().ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn read_cursor_cli(observed: i64) -> Option<Usage> {
    let program = crate::setup::find_program("agent").or_else(|| crate::setup::find_program("cursor-agent"))?;
    let status = run_bounded_stdout(&program, &["status", "--format", "json"], PROBE_TIMEOUT)
        .and_then(|out| serde_json::from_str::<Value>(&out).ok());
    let about = run_bounded_stdout(&program, &["about", "--format", "json"], PROBE_TIMEOUT)
        .and_then(|out| serde_json::from_str::<Value>(&out).ok());
    Usage::cursor_cli(status.as_ref(), about.as_ref(), observed)
}

fn read_cursor() -> Option<Usage> {
    let observed = now();
    if let Some(token) = cursor_token()
        && let Some(base) = cursor_api_base(std::env::var("CURSOR_API_ENDPOINT").ok().as_deref())
    {
        let agent = usage_http_agent();
        let me = cursor_rpc(&agent, &base, "GetMe", &token, &json!({}));
        let team_id = me
            .as_ref()
            .and_then(|m| m.get("teamId"))
            .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse::<i64>().ok()));
        let req = match team_id {
            Some(id) => json!({ "teamId": id, "includePooledUsage": true }),
            None => json!({}),
        };
        let period = cursor_rpc(&agent, &base, "GetCurrentPeriodUsage", &token, &req);
        if let Some(usage) = Usage::cursor(me.as_ref(), period.as_ref(), observed) {
            return Some(usage);
        }
    }
    read_cursor_cli(observed)
}

/// Account usage read from running sandboxes, whose agents reach neither ATOMICS nor this machine's
/// Codex logs: one `sbx exec` per sandbox, at most once per [`POLL_INTERVAL`], off the UI thread.
pub struct SandboxPull {
    tx: Sender<Report>,
    pulled: Option<Instant>,
    busy: Arc<AtomicBool>,
}

impl SandboxPull {
    pub fn new(tx: Sender<Report>) -> Self {
        Self { tx, pulled: None, busy: Arc::default() }
    }

    /// `names` are sandboxes whose agent is attached: `sbx exec` would start a stopped one.
    pub fn poll(&mut self, sbx: &str, names: Vec<String>, window: &crate::host::Window) {
        if names.is_empty()
            || self.busy.load(Ordering::Acquire)
            || self.pulled.is_some_and(|at| at.elapsed() < POLL_INTERVAL)
        {
            return;
        }
        self.pulled = Some(Instant::now());
        self.busy.store(true, Ordering::Release);
        let (tx, busy, window, sbx) = (self.tx.clone(), self.busy.clone(), window.clone(), sbx.to_string());
        let spawned = std::thread::Builder::new().name("atomics-sandbox-usage".into()).spawn(move || {
            let sbx = crate::setup::resolve_program(&sbx);
            for name in names {
                let Ok(output) = crate::sandbox::read_usage(&sbx, &name) else { continue };
                for report in sandbox_reports(&output, now()) {
                    let _ = tx.send(report);
                }
            }
            busy.store(false, Ordering::Release);
            window.request_repaint();
        });
        if spawned.is_err() {
            self.busy.store(false, Ordering::Release);
        }
    }
}

/// Reports in [`atomics_core::isolation::usage_args`]' output. The sandbox's agent can write all of
/// it, so nothing may claim to be newer than now: that would hide every real report after it.
pub fn sandbox_reports(output: &str, now: i64) -> Vec<Report> {
    let mut parts: Vec<(&str, Vec<&str>)> = Vec::new();
    for line in output.lines() {
        match line.strip_prefix(atomics_core::isolation::USAGE_MARK) {
            Some(header) => parts.push((header, Vec::new())),
            None => {
                if let Some((_, lines)) = parts.last_mut() {
                    lines.push(line);
                }
            }
        }
    }
    let mut reports = Vec::new();
    for (header, lines) in parts {
        let mut words = header.split_whitespace();
        match words.next() {
            Some("codex") => {
                if let Some(mut usage) = codex_reports(lines.into_iter()) {
                    usage.observed_at = usage.observed_at.min(now);
                    for reading in usage.models.values_mut() {
                        reading.observed_at = reading.observed_at.min(now);
                    }
                    reports.push(Report::Codex(usage));
                }
            }
            Some("claude") => {
                let Some(at) = words.next().and_then(|word| word.parse::<i64>().ok()) else { continue };
                let Ok(payload) = serde_json::from_str::<serde_json::Value>(&lines.join("\n")) else { continue };
                let usage = Usage::claude(&payload, at.min(now));
                if usage.session.is_some() || usage.weekly.is_some() || !usage.models.is_empty() {
                    reports.push(Report::Claude(usage));
                }
            }
            _ => {}
        }
    }
    reports
}

fn read_codex(sessions: &Path) -> Option<Usage> {
    let path = newest_rollout(sessions)?;
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let offset = length.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(offset.saturating_sub(1))).ok()?;
    let mut preceding = *b"\n";
    if offset > 0 {
        file.read_exact(&mut preceding).ok()?;
    }
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    let tail = String::from_utf8_lossy(&bytes);
    let tail = if preceding[0] != b'\n' { tail.split_once('\n')?.1 } else { &tail };
    codex_reports(tail.lines())
}

/// The tail may alternate between account and model buckets; keep the latest of each.
fn codex_reports<'a>(lines: impl Iterator<Item = &'a str>) -> Option<Usage> {
    let mut reports = lines.filter_map(Usage::codex);
    let mut usage = reports.next()?;
    for latest in reports {
        usage.update(latest);
    }
    Some(usage)
}

fn newest_rollout(sessions: &Path) -> Option<PathBuf> {
    let mut dirs = vec![sessions.to_path_buf()];
    // Codex stores logs under YYYY/MM/DD. Ignore symlinks and unrelated files.
    for _ in 0..3 {
        dirs = dirs
            .iter()
            .filter_map(|dir| std::fs::read_dir(dir).ok())
            .flatten()
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.path())
            .collect();
    }
    dirs.iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter(|entry| {
            entry.file_name().to_str().is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
        })
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .max()
        .map(|(_, path)| path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn copilot_jsonc_config_and_endpoint_url_resolution() {
        let config = "// User settings\n// Generated by Copilot CLI\n{\n  \"lastLoggedInUser\": {\"host\": \"https://github.com\", \"login\": \"octocat_acme\"},\n  \"copilotTokens\": {\"https://github.com:octocat_acme\": \"tok_fixture\"}\n}\n";
        let auth = parse_copilot_config(config);
        assert_eq!(auth.host, "https://github.com");
        assert_eq!(auth.login.as_deref(), Some("octocat_acme"));
        assert_eq!(auth.token.as_deref().map(String::as_str), Some("tok_fixture"));

        assert_eq!(
            copilot_user_url("https://github.com").as_deref(),
            Some("https://api.github.com/copilot_internal/user")
        );
        assert_eq!(
            copilot_user_url("https://acme.ghe.com/").as_deref(),
            Some("https://api.acme.ghe.com/copilot_internal/user")
        );
        assert_eq!(
            copilot_user_url("https://git.corp.example").as_deref(),
            Some("https://git.corp.example/api/v3/copilot_internal/user")
        );
        assert!(copilot_user_url("http://github.com").is_none());
        assert!(copilot_user_url("https://user@github.com").is_none());
    }

    #[test]
    fn cursor_auth_secret_and_endpoint_validation() {
        assert_eq!(
            parse_cursor_auth_secret(r#"{"accessToken":"jwt-fixture","refreshToken":"ref"}"#)
                .as_deref()
                .map(String::as_str),
            Some("jwt-fixture")
        );
        assert_eq!(
            parse_cursor_auth_secret(r#"{"apiKey":"key-fixture"}"#).as_deref().map(String::as_str),
            Some("key-fixture")
        );
        assert_eq!(parse_cursor_auth_secret("raw-token\n").as_deref().map(String::as_str), Some("raw-token"));
        assert!(parse_cursor_auth_secret("   ").is_none());

        assert_eq!(cursor_api_base(None).as_deref(), Some("https://api2.cursor.sh"));
        assert_eq!(cursor_api_base(Some("https://api2.cursor.sh/")).as_deref(), Some("https://api2.cursor.sh"));
        assert!(cursor_api_base(Some("http://api2.cursor.sh")).is_none());
        assert!(cursor_api_base(Some("https://evil@api2.cursor.sh")).is_none());
    }

    #[test]
    fn rollout_tail_and_sandbox_keep_separate_model_buckets() {
        let event = |id: &str, used: u32| {
            json!({"timestamp":"2026-10-28T15:00:00Z", "type":"event_msg", "payload": {
                "type":"token_count", "rate_limits":{"limit_id":id,
                    "primary":{"used_percent":used,"window_minutes":300,"resets_at":2000000000}}
            }})
            .to_string()
        };
        let content = [event("codex", 10), event("codex_other", 25), event("codex_premium", 80)].join("\n");
        let dir = crate::test_support::tempdir().unwrap();
        let day = dir.path().join("2026/10/28");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("rollout-models.jsonl"), &content).unwrap();
        let usage = read_codex(dir.path()).unwrap();
        assert_eq!(usage.session.unwrap().left_percent, 90.0);
        assert_eq!(usage.models["codex_other"].session.as_ref().unwrap().left_percent, 75.0);
        assert_eq!(usage.models["codex_premium"].session.as_ref().unwrap().left_percent, 20.0);
        let reports = sandbox_reports(&format!("{}codex\n{content}", atomics_core::isolation::USAGE_MARK), 100);
        let Report::Codex(usage) = &reports[0] else { panic!("Codex report") };
        assert_eq!(usage.models.len(), 2);
        assert!(usage.models.values().all(|reading| reading.observed_at == 100));
    }

    #[test]
    fn draining_reports_keeps_the_latest_account_values_and_ignores_duplicates() {
        let (tx, reports) = channel();
        let (control, _) = channel();
        let mut poller = UsagePoller {
            claude: Usage::default(),
            codex: Usage::default(),
            codex_chief: Usage::default(),
            copilot: Usage::default(),
            cursor: Usage::default(),
            reports,
            active: Arc::new(AtomicU8::new(0)),
            control,
        };
        let latest = Usage { observed_at: 100, ..Default::default() };
        tx.send(Report::Claude(latest.clone())).unwrap();
        tx.send(Report::Codex(latest.clone())).unwrap();
        tx.send(Report::Copilot(latest.clone())).unwrap();
        tx.send(Report::Cursor(latest.clone())).unwrap();
        assert!(poller.drain());
        tx.send(Report::Claude(Usage { observed_at: 99, ..Default::default() })).unwrap();
        tx.send(Report::Codex(latest.clone())).unwrap();
        tx.send(Report::Copilot(latest.clone())).unwrap();
        tx.send(Report::Cursor(latest.clone())).unwrap();
        assert!(!poller.drain());
        assert_eq!(poller.claude, latest);
        assert_eq!(poller.codex, latest);
        assert_eq!(poller.copilot, latest);
        assert_eq!(poller.cursor, latest);
    }

    #[test]
    fn poller_runs_off_thread_rechecks_and_does_not_repaint_unchanged_data() {
        let dir = crate::test_support::tempdir().unwrap();
        let day = dir.path().join("2026/10/28");
        std::fs::create_dir_all(&day).unwrap();
        let path = day.join("rollout-test.jsonl");
        let event = json!({"timestamp":"2026-10-28T15:00:00Z","type":"event_msg","payload":{
            "type":"token_count","rate_limits":{"primary":{"used_percent":15,"window_minutes":300,"resets_at":1793207640}}
        }});
        std::fs::write(&path, event.to_string()).unwrap();
        let ctx = egui::Context::default();
        let repaints = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = repaints.clone();
        ctx.set_request_repaint_callback(move |_| {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        // Clear egui's initial repaint before observing the worker.
        ctx.run_ui(Default::default(), |_| {}).drop_without_applying_deltas();
        ctx.run_ui(Default::default(), |_| {}).drop_without_applying_deltas();
        let (tx, rx) = channel();
        let (control, signals) = channel();
        let active = Arc::new(AtomicU8::new(0));
        let sessions = dir.path().to_path_buf();
        let worker = std::thread::spawn(move || {
            poll_worker(
                &sessions,
                None,
                active,
                tx,
                signals,
                crate::host::Window::attach(ctx),
                Duration::from_millis(20),
            )
        });
        assert!(matches!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), Report::Codex(_)));
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err(), "idle polls must not send data");
        let before = repaints.load(std::sync::atomic::Ordering::SeqCst);
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        assert_eq!(repaints.load(std::sync::atomic::Ordering::SeqCst), before);
        let mut changed = event;
        changed["payload"]["rate_limits"]["primary"]["used_percent"] = json!(25);
        std::fs::write(path, changed.to_string()).unwrap();
        let Report::Codex(usage) = rx.recv_timeout(Duration::from_secs(2)).unwrap() else { panic!("codex report") };
        assert_eq!(usage.session.unwrap().left_percent, 75.0);
        control.send(PollSignal::Stop).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn sandbox_reports_read_both_agents_and_never_claim_to_be_newer_than_now() {
        use atomics_core::isolation::USAGE_MARK;
        let codex = r#"{"timestamp":"2026-10-30T11:32:20.000Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":12.0,"window_minutes":10080,"resets_at":1793864523}}}}"#;
        let claude = r#"{"model":{"id":"claude-sonnet-5-5"},"rate_limits":{"five_hour":{"used_percentage":40,"resets_at":1793370000},"seven_day":{"used_percentage":10,"resets_at":1793800000}}}"#;
        let now = 1_793_361_000;
        // tail -c starts mid-line; the newest complete event wins.
        let output = format!("{USAGE_MARK}codex\n{{\"cut off\n{codex}\n\n{USAGE_MARK}claude 1793360900\n{claude}\n");
        let reports = sandbox_reports(&output, now);
        let [Report::Codex(codex_usage), Report::Claude(claude_usage)] = reports.as_slice() else {
            panic!("a Codex report, then a Claude report");
        };
        assert_eq!((codex_usage.weekly.as_ref().unwrap().left_percent, codex_usage.observed_at), (88.0, 1_793_359_940));
        assert_eq!(claude_usage.session.as_ref().unwrap().left_percent, 60.0);
        assert_eq!(claude_usage.weekly.as_ref().unwrap().left_percent, 90.0);
        assert_eq!(claude_usage.observed_at, 1_793_360_900, "the status line file's time, not the read's");

        // The agent writes all of it: a future time would hide every real report after it.
        let future = format!(
            "{USAGE_MARK}codex\n{}\n{USAGE_MARK}claude 4102444800\n{claude}\n",
            codex.replace("2026-10-30", "2099-01-01")
        );
        let reports = sandbox_reports(&future, now);
        assert_eq!(reports.len(), 2);
        for report in &reports {
            let (Report::Codex(usage)
            | Report::Claude(usage)
            | Report::CodexChief(usage)
            | Report::Copilot(usage)
            | Report::Cursor(usage)) = report;
            assert_eq!(usage.observed_at, now);
        }

        // A mark inside a line starts nothing; parts without limits, times or JSON report nothing.
        assert!(sandbox_reports(&format!("noise {USAGE_MARK}claude 5\n{claude}\n"), now).is_empty());
        let empty = format!(
            "{USAGE_MARK}claude 5\n{{\"model\":{{}}}}\n{USAGE_MARK}claude\n{claude}\n{USAGE_MARK}codex\nnot json\n"
        );
        assert!(sandbox_reports(&empty, now).is_empty());
    }

    #[test]
    fn log_reader_uses_newest_rollout_and_bounded_tail() {
        let dir = crate::test_support::tempdir().unwrap();
        let day = dir.path().join("2026/10/28");
        std::fs::create_dir_all(&day).unwrap();
        let event = json!({"timestamp":"2026-10-28T15:00:00Z","type":"event_msg","payload":{
            "type":"token_count","rate_limits":{"primary":{"used_percent":15,"window_minutes":300,"resets_at":1793207640}}
        }}).to_string();
        let old = day.join("rollout-old.jsonl");
        std::fs::write(&old, &event).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(1))
            .unwrap();
        let newest = day.join("rollout-new.jsonl");
        std::fs::write(&newest, format!("{event}\n{}\n{{\"partial\":", "x".repeat(TAIL_BYTES as usize))).unwrap();
        assert_eq!(newest_rollout(dir.path()).unwrap(), newest);
        assert!(read_codex(dir.path()).is_none(), "must not read an old event outside the tail");
        std::fs::write(&newest, format!("{}\n{event}\n{{\"partial\":", "x".repeat(TAIL_BYTES as usize))).unwrap();
        assert_eq!(read_codex(dir.path()).unwrap().session.unwrap().left_percent, 85.0);
        assert!(read_codex(&dir.path().join("missing")).is_none());
    }

    #[test]
    fn log_reader_keeps_a_complete_event_at_the_tail_boundary() {
        let dir = crate::test_support::tempdir().unwrap();
        let day = dir.path().join("2026/10/28");
        std::fs::create_dir_all(&day).unwrap();
        let event: serde_json::Value =
            serde_json::from_str(include_str!("../../atomics-core/tests/fixtures/codex-token-count-weekly-only.json"))
                .unwrap();
        let event = event.to_string();
        let tail = format!("{event}\n{}", " ".repeat(TAIL_BYTES as usize - event.len() - 1));
        assert_eq!(tail.len(), TAIL_BYTES as usize);
        let path = day.join("rollout-boundary.jsonl");
        std::fs::write(&path, format!("ignored prefix\n{tail}")).unwrap();
        assert_eq!(read_codex(dir.path()).unwrap().weekly.unwrap().left_percent, 90.0);
        std::fs::write(&path, format!("ignored prefix{tail}")).unwrap();
        assert!(read_codex(dir.path()).is_none(), "a fragment at the boundary is not an event");
    }
}
