//! Repository-scoped GitHub grants. Account values stay in the keychain or private broker.
use super::*;
use serde::{Deserialize, Serialize};

fn refuse_default_branch() -> bool {
    true
}

pub const ACCOUNT_PREFIX: &str = "GITHUB_ACCOUNT_";

pub fn account_name(account: &str) -> Result<String> {
    anyhow::ensure!(
        !account.is_empty() && account.len() <= 39 && account.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "Invalid GitHub account name"
    );
    Ok(format!(
        "{ACCOUNT_PREFIX}{}",
        account.to_ascii_lowercase().bytes().map(|b| format!("{b:02X}")).collect::<String>()
    ))
}

pub fn account_login(name: &str) -> Option<String> {
    let encoded = name.strip_prefix(ACCOUNT_PREFIX)?;
    if encoded.len() % 2 != 0 || !encoded.is_ascii() {
        return None;
    }
    let bytes = (0..encoded.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&encoded[i..i + 2], 16).ok())
        .collect::<Option<Vec<_>>>()?;
    let account = String::from_utf8(bytes).ok()?;
    (account_name(&account).ok()?.as_str() == name).then_some(account)
}

pub fn account_spec(account: &str) -> Result<atomics_core::secrets::Secret> {
    Ok(atomics_core::secrets::Secret {
        name: account_name(account)?,
        host: Some("github.com".into()),
        auth: atomics_core::secrets::Auth::Basic("x-access-token".into()),
        methods: vec!["GET".into()],
        path_prefix: Some("/__atomics_github_account".into()),
        ..Default::default()
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub repository: String,
    /// Include the repository’s pull-request endpoints, as sessions and publishers do.
    pub pulls: bool,
    #[serde(default = "refuse_default_branch")]
    pub refuse_default_branch: bool,
}

impl Scope {
    pub fn validate(&self) -> Result<()> {
        let parts: Vec<_> = self.repository.split('/').collect();
        anyhow::ensure!(
            parts.len() == 2
                && parts.iter().all(|s| !s.is_empty()
                    && *s != "."
                    && *s != ".."
                    && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))),
            "Invalid GitHub repository"
        );
        Ok(())
    }

    pub fn from_project(project: &std::path::Path, pulls: bool) -> Result<Self> {
        let remote = atomics_core::git::git(project, &["remote", "get-url", "origin"])?;
        let remote = remote.trim();
        let path = remote
            .strip_prefix("https://github.com/")
            .or_else(|| remote.strip_prefix("git@github.com:"))
            .or_else(|| remote.strip_prefix("ssh://git@github.com/"))
            .ok_or_else(|| anyhow::anyhow!("Project origin must be a GitHub repository"))?;
        let scope =
            Self { repository: path.strip_suffix(".git").unwrap_or(path).into(), pulls, refuse_default_branch: true };
        scope.validate()?;
        Ok(scope)
    }

    pub(super) fn allows(&self, method: &str, route: &Route) -> bool {
        let git = format!("/{}.git", self.repository);
        if route.host == "github.com" {
            return match (method, route.path.strip_prefix(&git), route.query.as_deref()) {
                ("GET", Some("/info/refs"), Some("service=git-upload-pack" | "service=git-receive-pack")) => true,
                ("POST", Some("/git-upload-pack" | "/git-receive-pack"), None) => true,
                _ => false,
            };
        }
        if !self.pulls || route.host != "api.github.com" {
            return false;
        }
        let pulls = format!("/repos/{}/pulls", self.repository);
        if route.path == pulls {
            return matches!(method, "GET" | "POST");
        }
        method == "PATCH"
            && route.query.is_none()
            && route
                .path
                .strip_prefix(&format!("{pulls}/"))
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    }
}

impl Credential {
    pub(super) fn github(account: &str, value: String, scope: Scope) -> Result<Self> {
        scope.validate()?;
        let mut credential = Self::secret(&account_spec(account)?, value);
        credential.hosts = vec!["github.com".into(), "api.github.com".into()];
        credential.github = Some(scope);
        Ok(credential)
    }
}

impl Inner {
    /// Fetch the current default branch privately on every push; never trust a session's refs.
    pub(super) fn github_push_allowed(&self, credential: &Credential, body: &[u8]) -> bool {
        let Some(scope) = &credential.github else {
            return false;
        };
        if !scope.refuse_default_branch {
            return receive_pack_allowed(body, "");
        }
        let Source::Static(token) = &credential.source else {
            return true;
        }; // broker repeats the check
        let base = self.upstreams.get("api.github.com").cloned().unwrap_or_else(|| "https://api.github.com".into());
        let result = self
            .agent
            .get(format!("{base}/repos/{}", scope.repository))
            .header("authorization", format!("Bearer {token}"))
            .header("user-agent", "atomics")
            .call();
        let Ok(mut response) = result else {
            return false;
        };
        if !response.status().is_success() {
            return false;
        }
        let Ok(value) = response
            .body_mut()
            .with_config()
            .limit(1024 * 1024)
            .read_to_vec()
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .ok_or(())
        else {
            return false;
        };
        let Some(branch) = value["default_branch"].as_str().filter(|b| !b.is_empty()) else {
            return false;
        };
        receive_pack_allowed(body, branch)
    }
}

/// Only ordinary branch updates are supported. Reject malformed packets, signed pushes,
/// tags, alternate ref namespaces and the default branch, before sending any bytes upstream.
fn receive_pack_allowed(mut bytes: &[u8], default: &str) -> bool {
    let mut commands = 0;
    loop {
        let Some(prefix) = bytes.get(..4).and_then(|p| std::str::from_utf8(p).ok()) else {
            return false;
        };
        let Ok(len) = usize::from_str_radix(prefix, 16) else {
            return false;
        };
        if len == 0 {
            return commands > 0;
        }
        if len < 4 || len > bytes.len() {
            return false;
        }
        let Ok(line) = std::str::from_utf8(&bytes[4..len]) else {
            return false;
        };
        let line = line.split('\0').next().unwrap_or_default().trim_end_matches('\n');
        let parts: Vec<_> = line.split(' ').collect();
        if parts.len() != 3
            || !parts[..2].iter().all(|oid| oid.len() == 40 && oid.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return false;
        }
        let Some(branch) = parts[2].strip_prefix("refs/heads/") else {
            return false;
        };
        if branch.is_empty() || branch == default {
            return false;
        }
        commands += 1;
        bytes = &bytes[len..];
    }
}

impl Service {
    /// Session preparation checks the account's metadata scope before issuing a repository grant.
    pub fn grant_project_github(
        &self,
        session: &str,
        account: &str,
        project: &std::path::Path,
        workspace: Option<&str>,
        sandbox: bool,
        granted: Option<Granted>,
    ) -> Result<Option<Granted>> {
        let result = (|| {
            let root = atomics_core::workspace::project_root(project)?;
            let name = account_name(account)?;
            let specs = if let Some(client) = &self.broker {
                client.list()?
            } else {
                crate::credentials::Store::local(self.keychain.clone()).secrets()?
            };
            let spec = specs
                .into_iter()
                .find(|s| s.name == name)
                .ok_or_else(|| anyhow::anyhow!("Connect the project's GitHub account first"))?;
            anyhow::ensure!(
                spec.scope.includes(workspace, root.to_str()),
                "GitHub account is outside this project's secret scope"
            );
            self.grant_github(session, account, Scope::from_project(&root, true)?, sandbox, granted.clone())
        })();
        if result.is_err()
            && let Some(granted) = &granted
        {
            self.revoke(granted);
        }
        result
    }

    pub fn grant_github(
        &self,
        session: &str,
        account: &str,
        mut scope: Scope,
        sandbox: bool,
        granted: Option<Granted>,
    ) -> Result<Option<Granted>> {
        scope.validate()?;
        let _edit = self.edits.lock().unwrap();
        let name = account_name(account)?;
        let mut credential = if let Some(client) = &self.broker {
            anyhow::ensure!(
                client.list()?.iter().any(|s| s.name == name),
                "Connect the project's GitHub account first"
            );
            let mut credential = Credential::github(account, String::new(), scope.clone())?;
            credential.source = Source::Broker(client.clone());
            credential
        } else {
            let spec = account_spec(account)?;
            let (stored_spec, value) = crate::credentials::Store::local(self.keychain.clone())
                .secret_values()?
                .into_iter()
                .find(|(s, _)| {
                    s.name == spec.name
                        && s.host == spec.host
                        && s.auth == spec.auth
                        && s.path_prefix == spec.path_prefix
                })
                .ok_or_else(|| anyhow::anyhow!("Connect the project's GitHub account first"))?;
            scope.refuse_default_branch = stored_spec.refuse_default_branch;
            Credential::github(account, value.to_string(), scope.clone())?
        };
        // Retain the immutable policy in both processes.
        credential.github = Some(scope.clone());
        let mut relay = self.relay.lock().unwrap();
        if relay.is_none() {
            *relay = Some(self.start()?);
        }
        let relay = relay.as_ref().unwrap();
        let placeholder = relay.grant(session, Arc::new(credential));
        if let Some(client) = &self.broker
            && let Err(error) = client.grant_github(account, session, &placeholder, scope.clone())
        {
            relay.revoke(&placeholder);
            return Err(error);
        }
        let mut grant = granted.unwrap_or_default();
        grant.port = relay.port();
        grant.secret_placeholders.push(placeholder.clone());
        let address = if sandbox { atomics_core::isolation::RELAY_HOST } else { "127.0.0.1" };
        let base = format!("http://{address}:{}/github.com/", relay.port());
        let mut config = vec![
            (format!("url.{base}.insteadOf"), "https://github.com/".into()),
            (format!("url.{base}.insteadOf"), "git@github.com:".into()),
            (format!("url.{base}.insteadOf"), "ssh://git@github.com/".into()),
            (format!("http.{base}.extraHeader"), format!("Authorization: Bearer {placeholder}")),
            ("credential.helper".into(), String::new()),
            ("http.followRedirects".into(), "false".into()),
            ("push.autoSetupRemote".into(), "true".into()),
            ("push.default".into(), "current".into()),
        ];
        let target = format!("{base}{}.git", scope.repository);
        for prefix in ["https://github.com/", "git@github.com:", "ssh://git@github.com/"] {
            for suffix in ["", ".git"] {
                config.push((format!("url.{target}.insteadOf"), format!("{prefix}{}{suffix}", scope.repository)));
            }
        }
        grant.secrets.insert("GIT_CONFIG_COUNT".into(), config.len().to_string());
        grant.secrets.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
        for (i, (key, value)) in config.into_iter().enumerate() {
            grant.secrets.insert(format!("GIT_CONFIG_KEY_{i}"), key);
            grant.secrets.insert(format!("GIT_CONFIG_VALUE_{i}"), value);
        }
        grant
            .secrets
            .insert("ATOMICS_GITHUB_API_URL".into(), format!("http://{address}:{}/api.github.com", relay.port()));
        grant.secrets.insert("ATOMICS_GITHUB_PLACEHOLDER".into(), placeholder);
        Ok(Some(grant))
    }
}

#[cfg(test)]
pub(crate) mod tests;
