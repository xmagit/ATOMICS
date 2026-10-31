//! Host-only Azure CLI token acquisition. stdout stays in a pipe and in memory; in particular,
//! do not use sandbox::sbx_output here, which captures command output in temporary files.

use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use atomics_core::isolation::AzureConfig;

const REFRESH: Duration = Duration::from_secs(120);
const TIMEOUT: Duration = Duration::from_secs(60);

/// Status contains no account identifiers or tokens, even when az prints them unexpectedly.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub found: bool,
    pub logged_in: bool,
}

pub fn status(program: Option<PathBuf>) -> Status {
    let Some(program) = program else { return Status::default() };
    let mut command = Command::new(program);
    command
        .args(["account", "show", "--output", "none", "--only-show-errors"])
        .env("AZURE_LOGGING_ENABLE_LOG_FILE", "false")
        .env("AZURE_CORE_COLLECT_TELEMETRY", "false")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let Ok(mut child) = command.spawn() else { return Status { found: true, logged_in: false } };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(result)) => return Status { found: true, logged_in: result.success() },
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                // SAFETY: the child owns this process group.
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.wait();
                return Status { found: true, logged_in: false };
            }
        }
    }
}

pub fn doctor_line(enabled: bool) -> String {
    if !enabled {
        return "Azure CLI relay: off; enable in Settings → Credentials".into();
    }
    let status = status(if cfg!(test) { None } else { crate::setup::find_program("az") });
    match (status.found, status.logged_in) {
        (false, _) => "✗ Azure CLI relay: enabled, az not found (warning); install az on the host".into(),
        (true, false) => "✗ Azure CLI relay: enabled, not logged in (warning); run az login on the host".into(),
        (true, true) => "✓ Azure CLI relay: enabled, az found, logged in (host login; tokens stay on host)".into(),
    }
}

pub struct Azure {
    program: PathBuf,
    pub config: AzureConfig,
    tokens: Mutex<HashMap<String, Token>>,
}

struct Token {
    secret: String,
    expires: SystemTime,
}

impl Azure {
    pub fn current(config: AzureConfig) -> Option<Self> {
        config.enabled.then(|| crate::setup::find_program("az")).flatten().map(|program| Self::new(program, config))
    }

    pub fn new(program: PathBuf, config: AzureConfig) -> Self {
        Self { program, config, tokens: Mutex::default() }
    }

    /// Serialize refreshes so concurrent agents share a single token for each audience. Neither
    /// the grants lock nor the app's state lock is held while Azure CLI runs.
    pub fn token(&self, host: &str) -> Result<String, &'static str> {
        self.token_at(host, SystemTime::now())
    }

    fn token_at(&self, host: &str, now: SystemTime) -> Result<String, &'static str> {
        let resource = self.config.resources.get(host).ok_or("Azure host is not configured")?;
        let mut tokens = self.tokens.lock().unwrap();
        if let Some(token) = tokens.get(resource)
            && token.expires.duration_since(now).unwrap_or_default() > REFRESH
        {
            return Ok(token.secret.clone());
        }
        // Never fall back to an old token after a failed refresh.
        tokens.remove(resource);
        let output = self.output(resource, TIMEOUT)?;
        let value: serde_json::Value =
            serde_json::from_slice(&output).map_err(|_| "Azure CLI returned invalid JSON")?;
        let secret = value["accessToken"]
            .as_str()
            .filter(|s| {
                !s.is_empty()
                    && s.len() < 64 * 1024
                    && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.~+/=".contains(&b))
            })
            .ok_or("Azure CLI returned no usable access token")?;
        // Azure CLI >= 2.54 supplies an unambiguous UTC POSIX expiry on both OSes. Refuse older
        // local-time expiresOn values rather than guessing across timezones/DST boundaries.
        let expiry = value["expires_on"]
            .as_u64()
            .or_else(|| value["expires_on"].as_str()?.parse().ok())
            .and_then(|secs| UNIX_EPOCH.checked_add(Duration::from_secs(secs)))
            .filter(|expires| expires.duration_since(now).unwrap_or_default() > REFRESH)
            .ok_or("Azure CLI needs expires_on with more than two minutes remaining; update az or log in again")?;
        let secret = secret.to_string();
        tokens.insert(resource.clone(), Token { secret: secret.clone(), expires: expiry });
        Ok(secret)
    }

    fn output(&self, resource: &str, timeout: Duration) -> Result<Vec<u8>, &'static str> {
        let mut command = Command::new(&self.program);
        command
            .args(["account", "get-access-token", "--resource", resource, "--output", "json", "--only-show-errors"])
            .env("AZURE_LOGGING_ENABLE_LOG_FILE", "false")
            .env("AZURE_CORE_COLLECT_TELEMETRY", "false")
            .env("AZURE_CORE_ONLY_SHOW_ERRORS", "true");
        if let Some(path) = crate::setup::SHELL_PATH.as_ref() {
            command.env("PATH", path);
        }
        use crate::sandbox::setup::{OutputError, private_output};
        let output = private_output(&mut command, timeout).map_err(|error| match error {
            OutputError::Start => "could not start host Azure CLI",
            OutputError::Read => "could not read Azure CLI output",
            OutputError::Timeout => "host Azure CLI timed out",
            OutputError::TooLarge => "Azure CLI output was too large",
        })?;
        if !output.status.success() {
            return Err("host Azure CLI failed; run az login on the host");
        }
        Ok(output.stdout)
    }

    pub fn read(&self, host: &str, method: &str, path: &str) -> bool {
        method == "GET"
            || (self.config.query_read_exception
                && host == "api.applicationinsights.io"
                && method == "POST"
                && path
                    .strip_prefix("/v1/apps/")
                    .and_then(|p| p.strip_suffix("/query"))
                    .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok() && !id.contains('/')))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Arc;

    pub fn fake(dir: &std::path::Path) -> PathBuf {
        let program = dir.join("az");
        std::fs::copy(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/az"), &program).unwrap();
        expiry(dir, SystemTime::now() + Duration::from_secs(3600));
        program
    }

    fn expiry(dir: &std::path::Path, time: SystemTime) {
        std::fs::write(dir.join("az-expiry"), time.duration_since(UNIX_EPOCH).unwrap().as_secs().to_string()).unwrap();
    }

    #[test]
    fn azure_session_is_opt_in_and_status_never_includes_account_data() {
        use super::super::Service;
        let (dir, keychain, store) = crate::credentials::tests::machine(cfg!(target_os = "macos"));
        let program = fake(dir.path());
        let enabled = AzureConfig { enabled: true, ..Default::default() };
        let service = Service::transient(dir.path().join("state"), keychain)
            .with_azure(Azure::new(program.clone(), enabled.clone()));
        let off = AzureConfig::default();
        assert!(!off.enabled);
        assert!(service.grant_session("off", false, &off).unwrap().is_none());
        assert_eq!(service.live_grants(), 0);
        assert!(!dir.path().join("az-argv").exists());
        store.put("claude", "sk-ant-oat01-PRIVATE", None).unwrap();
        let claude = service.grant_session("claude-only", true, &off).unwrap().unwrap();
        assert!(claude.azure.is_none() && claude.azure_env().is_empty());
        service.revoke(&claude);
        let grant = service.grant_session("on", false, &enabled).unwrap().unwrap();
        assert!(grant.azure.as_deref().unwrap().starts_with("atomics-azure-"));
        assert_eq!(grant.azure_env().len(), 2);
        assert_eq!(service.usage("Azure CLI").1, ["on"]);
        assert!(!dir.path().join("az-argv").exists(), "granting must not fetch tokens");
        service.revoke_azure();
        assert!(service.usage("Azure CLI").1.is_empty());
        let logged_in = status(Some(program.clone()));
        assert!(logged_in.found && logged_in.logged_in);
        std::fs::write(dir.path().join("az-fail"), "").unwrap();
        let logged_out = status(Some(program));
        assert!(logged_out.found && !logged_out.logged_in);
        assert!(!status(None).found);
        let output = format!("{logged_in:?} {logged_out:?} {}", doctor_line(false));
        assert!(!output.contains("PRIVATE") && !output.contains("AZURE-REAL"));
        assert!(output.contains("off"));
        assert!(!std::fs::read_to_string(dir.path().join("az-argv")).unwrap().contains("get-access-token"));
    }

    #[test]
    fn azure_is_found_on_the_login_shell_path_on_both_hosts() {
        use std::os::unix::fs::PermissionsExt;
        const CHILD: &str = "ATOMICS_AZURE_DISCOVERY_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let azure =
                Azure::current(AzureConfig { enabled: true, ..Default::default() }).expect("az on login-shell PATH");
            assert_eq!(azure.token("api.applicationinsights.io").unwrap(), "AZURE-REAL-1");
            assert!(Azure::current(AzureConfig { enabled: false, ..Default::default() }).is_none());
            return;
        }
        let dir = crate::test_support::tempdir().unwrap();
        fake(dir.path());
        let shell = dir.path().join("shell");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{0}/shell-argv'\nprintf '__ATOMICS__{0}:/usr/bin:/bin__ATOMICS__'\n",
            dir.path().display()
        );
        std::fs::write(&shell, script).unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = crate::test_support::output(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "relay::azure::tests::azure_is_found_on_the_login_shell_path_on_both_hosts"])
                .env(CHILD, "1")
                .env("SHELL", shell)
                .env("PATH", "/usr/bin:/bin"),
        );
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(std::fs::read_to_string(dir.path().join("shell-argv")).unwrap().starts_with("-l -i -c"));
    }

    #[test]
    fn a_hung_azure_cli_and_its_helpers_are_killed_without_capturing_output() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_support::tempdir().unwrap();
        let program = dir.path().join("az");
        let marker = dir.path().join("helper-leaked");
        std::fs::write(&program, format!("#!/bin/sh\n(sleep 0.4; touch '{}') & wait\n", marker.display())).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let azure = Azure::new(program, AzureConfig::default());
        assert_eq!(
            azure.output("https://api.applicationinsights.io", Duration::from_millis(100)).unwrap_err(),
            "host Azure CLI timed out"
        );
        std::thread::sleep(Duration::from_millis(500));
        assert!(!marker.exists());
    }

    #[test]
    fn azure_cli_tokens_are_shared_by_audience_refreshed_before_expiry_and_never_captured_on_disk() {
        let dir = crate::test_support::tempdir().unwrap();
        let now = SystemTime::now();
        let azure = Arc::new(Azure::new(fake(dir.path()), AzureConfig::default()));
        let host = "api.applicationinsights.io";
        let requests: Vec<_> = (0..8)
            .map(|_| {
                let azure = azure.clone();
                std::thread::spawn(move || azure.token_at(host, now).unwrap())
            })
            .collect();
        for request in requests {
            assert_eq!(request.join().unwrap(), "AZURE-REAL-1");
        }
        assert_eq!(azure.token_at("management.azure.com", now).unwrap(), "AZURE-REAL-2");
        let later = now + Duration::from_secs(3500);
        expiry(dir.path(), later + Duration::from_secs(3600));
        assert_eq!(azure.token_at(host, later).unwrap(), "AZURE-REAL-3");
        std::fs::write(dir.path().join("az-fail"), "").unwrap();
        assert_eq!(
            azure.token_at(host, later + Duration::from_secs(3500)).unwrap_err(),
            "host Azure CLI failed; run az login on the host"
        );
        assert!(!azure.tokens.lock().unwrap().contains_key("https://api.applicationinsights.io"));
        assert!(azure.token("unconfigured.example").is_err());
        let args = std::fs::read_to_string(dir.path().join("az-argv")).unwrap();
        assert_eq!(args.lines().count(), 4);
        assert!(args.contains("account get-access-token --resource https://api.applicationinsights.io --output json"));
        assert!(args.contains("--resource https://management.azure.com/"));
        // Only the fake executable contains the token prefix; no command capture, token cache or log.
        for entry in std::fs::read_dir(dir.path()).unwrap().flatten().filter(|e| e.file_name() != "az") {
            assert!(!std::fs::read_to_string(entry.path()).unwrap().contains("AZURE-REAL"));
        }
    }

    #[test]
    fn azure_cli_rejects_bad_output_and_the_query_exception_is_precise_and_optional() {
        let dir = crate::test_support::tempdir().unwrap();
        let program = fake(dir.path());
        let mut azure = Azure::new(program, AzureConfig::default());
        for output in [
            "not-json",
            r#"{"accessToken":"PRIVATE","expiresOn":"2026-10-30 10:00:00"}"#,
            r#"{"accessToken":"bad\r\nheader","expires_on":9999999999}"#,
            r#"{"accessToken":"expired","expires_on":1}"#,
        ] {
            std::fs::write(dir.path().join("az-invalid"), output).unwrap();
            let error = azure.token("api.applicationinsights.io").unwrap_err();
            assert!(!error.contains("PRIVATE") && !error.contains(output));
        }
        let query = "/v1/apps/11111111-1111-1111-1111-111111111111/query";
        assert!(azure.read("management.azure.com", "GET", "/"));
        assert!(!azure.read("api.applicationinsights.io", "POST", query));
        azure.config.query_read_exception = true;
        assert!(azure.read("api.applicationinsights.io", "POST", query));
        for (host, method, path) in [
            ("management.azure.com", "POST", query),
            ("api.applicationinsights.io", "PUT", query),
            ("api.applicationinsights.io", "POST", "/v1/apps/x/query"),
            ("api.applicationinsights.io", "POST", "/v1/apps/11111111-1111-1111-1111-111111111111/query/extra"),
        ] {
            assert!(!azure.read(host, method, path));
        }
    }

    #[test]
    fn azure_relay_defaults_to_get_and_writes_wait_for_exact_approval_and_are_audited() {
        use super::super::{Credential, Relay, fake};
        use atomics_core::{approvals::Decision, hook::HookReply};
        let dir = crate::test_support::tempdir().unwrap();
        let program = self::fake(dir.path());
        let upstream = fake::start();
        let host = "api.applicationinsights.io";
        let log = dir.path().join("relay.log");
        let relay = Relay::start(0, log.clone(), [(host.into(), upstream.url.clone())].into()).unwrap();
        let azure = Arc::new(Azure::new(program.clone(), AzureConfig::default()));
        let placeholder = relay.grant("atomics-azure-agent", Arc::new(Credential::azure(azure)));
        let request = |method: &str, path: &str, body: &str| {
            format!(
                "{method} /{host}{path} HTTP/1.1\r\nauthorization: Bearer {placeholder}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
        };
        let get = request("GET", "/v1/models?private-query", "");
        let (head, mut reader) = fake::send(relay.port(), &get);
        assert!(head.starts_with("HTTP/1.1 200"));
        while !fake::chunk(&mut reader).is_empty() {}
        assert_eq!(upstream.seen.lock().unwrap()[0].header("authorization"), ["Bearer AZURE-REAL-1"]);

        let port = relay.port();
        for (method, decision, expected) in [
            ("POST", Decision::Deny, 403),
            ("PUT", Decision::Allow, 200),
            ("PATCH", Decision::AllowAlways, 200),
            ("DELETE", Decision::Deny, 403),
        ] {
            let text = request(method, "/v1/models?private-write", "private-body");
            let client = std::thread::spawn(move || {
                let (head, mut reader) = fake::send(port, &text);
                if head.starts_with("HTTP/1.1 200") {
                    while !fake::chunk(&mut reader).is_empty() {}
                }
                head
            });
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
            let approval = loop {
                if let Some(approval) = relay.inner.approvals.lock().unwrap().pop() {
                    break approval;
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            };
            assert_eq!(approval.session, "atomics-azure-agent");
            let command = approval.request.command.unwrap();
            assert!(command.contains(method) && command.contains("private-write") && command.contains("private-body"));
            assert!(!command.contains(&placeholder) && !command.contains("AZURE-REAL"));
            assert_eq!(
                upstream.seen.lock().unwrap().len(),
                if method == "DELETE" {
                    3
                } else if method == "PATCH" {
                    2
                } else {
                    1
                }
            );
            assert!(approval.reply.answer(HookReply { stdout: Some(decision.output(None)) }));
            assert!(client.join().unwrap().starts_with(&format!("HTTP/1.1 {expected}")));
        }
        // An explicitly configured query read exception requires no approval and carries only the
        // placeholder on the client side. Other hosts and methods are still denied.
        let config = AzureConfig { query_read_exception: true, ..AzureConfig::default() };
        let azure = Arc::new(Azure::new(program, config));
        let query_placeholder = relay.grant("atomics-query-agent", Arc::new(Credential::azure(azure)));
        let query = request("POST", "/v1/apps/11111111-1111-1111-1111-111111111111/query", "private-kql")
            .replace(&placeholder, &query_placeholder);
        upstream.release.send(()).unwrap();
        let (head, mut reader) = fake::send(port, &query);
        assert!(head.starts_with("HTTP/1.1 200"));
        while !fake::chunk(&mut reader).is_empty() {}
        assert!(relay.inner.approvals.lock().unwrap().is_empty());
        let forbidden = request("GET", "/v1/models", "").replace(host, "unconfigured.example");
        assert!(fake::send(port, &forbidden).0.starts_with("HTTP/1.1 403"));
        assert!(fake::send(port, &request("HEAD", "/v1/models", "")).0.starts_with("HTTP/1.1 403"));
        relay.revoke(&placeholder);
        assert!(fake::send(port, &get).0.starts_with("HTTP/1.1 401"));
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
        let text = loop {
            let text = std::fs::read_to_string(&log).unwrap();
            if text.lines().count() == 9 {
                break text;
            }
            assert!(Instant::now() < deadline, "{text}");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(text.contains("atomics-azure-agent api.applicationinsights.io PUT /v1/models 200"));
        assert!(text.contains("atomics-query-agent api.applicationinsights.io POST /v1/apps/"));
        for private in ["private-query", "private-write", "private-body", "private-kql", "AZURE-REAL", &placeholder] {
            assert!(!text.contains(private));
        }
    }

    #[test]
    fn azure_write_expiry_and_revocation_fail_closed_before_fetching_a_token() {
        use super::super::{Credential, Relay, fake};
        let dir = crate::test_support::tempdir().unwrap();
        let azure = Arc::new(Azure::new(self::fake(dir.path()), AzureConfig::default()));
        let relay = Relay::start(0, dir.path().join("relay.log"), HashMap::new()).unwrap();
        for revoke in [false, true] {
            let placeholder = relay.grant("atomics-worker", Arc::new(Credential::azure(azure.clone())));
            let port = relay.port();
            let request = format!(
                "POST /api.applicationinsights.io/v1/models HTTP/1.1\r\nauthorization: Bearer {placeholder}\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{{}}"
            );
            let client = std::thread::spawn(move || fake::send(port, &request).0);
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
            let approval = loop {
                if let Some(approval) = relay.inner.approvals.lock().unwrap().pop() {
                    break approval;
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            };
            if revoke {
                relay.revoke(&placeholder);
            } else {
                approval.reply.wait(Duration::ZERO);
            }
            assert!(client.join().unwrap().starts_with("HTTP/1.1 403"));
            assert!(!approval.reply.answer(atomics_core::hook::HookReply {
                stdout: Some(atomics_core::approvals::Decision::Allow.output(None))
            }));
        }
        assert!(!dir.path().join("az-argv").exists());
    }
}
