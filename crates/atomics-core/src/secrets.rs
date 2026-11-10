//! Value-free secret metadata, shared by native Settings and protocol clients.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "target", rename_all = "snake_case")]
pub enum Scope {
    #[default]
    Everywhere,
    Workspace(String),
    /// The absolute path of the project's main checkout, never a linked worktree.
    Project(String),
}

impl Scope {
    pub fn includes(&self, workspace: Option<&str>, project: Option<&str>) -> bool {
        match self {
            Self::Everywhere => true,
            Self::Workspace(name) => workspace == Some(name.as_str()),
            Self::Project(path) => project == Some(path.as_str()),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Everywhere => "Everywhere".into(),
            Self::Workspace(name) => format!("Workspace: {name}"),
            Self::Project(path) => format!("Project: {path}"),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum Auth {
    #[default]
    Bearer,
    Header(String),
    /// The value is the password; this is the public username.
    Basic(String),
    Query(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Secret {
    pub name: String,
    /// GitHub account secrets refuse default-branch pushes unless explicitly disabled.
    #[serde(default = "refuse_default_branch")]
    pub refuse_default_branch: bool,
    pub host: Option<String>,
    #[serde(default)]
    pub scope: Scope,
    #[serde(default)]
    pub auth: Auth,
    #[serde(default)]
    pub methods: Vec<String>,
    #[serde(default)]
    pub path_prefix: Option<String>,
}

fn refuse_default_branch() -> bool {
    true
}

impl Default for Secret {
    fn default() -> Self {
        Self {
            name: String::new(),
            host: None,
            scope: Scope::default(),
            auth: Auth::default(),
            methods: Vec::new(),
            path_prefix: None,
            refuse_default_branch: true,
        }
    }
}

pub const EXPOSED: &str = "Exposed — agents can read and leak this value";

impl Secret {
    pub fn kind(&self) -> &'static str {
        if self.host.is_some() { "Relayed" } else { EXPOSED }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if !valid_name(&self.name) {
            return Err(
                "Use an environment variable name (letters, digits and underscores; no leading digit or reserved runtime name)",
            );
        }
        if self.host.as_deref().is_some_and(|h| !valid_host(h)) {
            return Err("Use a DNS host only, such as api.example.com (no URL, port or wildcard)");
        }
        match &self.scope {
            Scope::Everywhere => {}
            Scope::Workspace(s)
                if !s.is_empty()
                    && s.len() <= 128
                    && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)) => {}
            Scope::Project(s) if s.starts_with('/') && s.len() <= 4096 && !s.chars().any(char::is_control) => {}
            _ => return Err("Select a workspace or an absolute main checkout path"),
        }
        match &self.auth {
            Auth::Bearer => {}
            Auth::Header(s)
                if !s.is_empty()
                    && s.len() <= 100
                    && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    && ![
                        "host",
                        "connection",
                        "content-length",
                        "transfer-encoding",
                        "cookie",
                        "proxy-authorization",
                        "accept-encoding",
                    ]
                    .iter()
                    .any(|h| s.eq_ignore_ascii_case(h)) => {}
            Auth::Query(s)
                if !s.is_empty()
                    && s.len() <= 100
                    && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)) => {}
            Auth::Basic(s) if s.len() <= 256 && !s.contains(':') && !s.chars().any(char::is_control) => {}
            _ => return Err("Invalid authentication header, query parameter or basic username"),
        }
        if self.methods.len() > 9
            || self.methods.iter().any(|s| {
                !["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "TRACE", "CONNECT"].contains(&s.as_str())
            })
        {
            return Err("Methods must be uppercase HTTP method names");
        }
        if self.path_prefix.as_deref().is_some_and(|s| !valid_path(s)) {
            return Err("Use a plain absolute path prefix without escapes, query, empty or dot segments");
        }
        Ok(())
    }
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name.bytes().enumerate().all(|(i, b)| b.is_ascii_alphabetic() || b == b'_' || i > 0 && b.is_ascii_digit())
        && !name.starts_with("ATOMICS_")
        && ![
            "PATH",
            "HOME",
            "SHELL",
            "ENV",
            "BASH_ENV",
            "LD_PRELOAD",
            "LD_LIBRARY_PATH",
            "DYLD_INSERT_LIBRARIES",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "SSH_AUTH_SOCK",
        ]
        .contains(&name)
}

pub fn valid_host(host: &str) -> bool {
    host.len() <= 253
        && host.contains('.')
        && host.parse::<std::net::IpAddr>().is_err()
        && host.split('.').all(|s| {
            !s.is_empty()
                && s.len() <= 63
                && !s.starts_with('-')
                && !s.ends_with('-')
                && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

fn valid_path(path: &str) -> bool {
    path.starts_with('/')
        && path.len() <= 2048
        && path.bytes().all(|b| b.is_ascii_graphic() && !b"%?#\\*".contains(&b))
        && (path == "/"
            || path[1..]
                .strip_suffix('/')
                .unwrap_or(&path[1..])
                .split('/')
                .all(|s| !s.is_empty() && s != "." && s != ".."))
}

/// Service names and common environment names. Unknown names leave the host empty.
pub fn service(name: &str) -> Option<(&'static str, Auth)> {
    Some(match name.to_ascii_lowercase().as_str() {
        "openai" | "openai_api_key" => ("api.openai.com", Auth::Bearer),
        "anthropic" | "anthropic_api_key" => ("api.anthropic.com", Auth::Header("x-api-key".into())),
        "claude" | "claude_code_oauth_token" => ("api.anthropic.com", Auth::Bearer),
        "github" | "github_token" | "gh_token" => ("api.github.com", Auth::Bearer),
        "linear" | "linear_api_key" => ("api.linear.app", Auth::Header("authorization".into())),
        "logo.dev" | "logo_dev_api_key" => ("api.logo.dev", Auth::Bearer),
        "stripe" | "stripe_secret_key" => ("api.stripe.com", Auth::Bearer),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secrets_metadata_validates_routes_scopes_and_does_not_debug_values() {
        let mut spec = Secret { name: "EXAMPLE_KEY".into(), ..Default::default() };
        assert!(spec.validate().is_ok());
        assert_eq!(spec.kind(), EXPOSED);
        for host in
            ["https://api.example.com", "127.0.0.1", "*.example.com", "api.example.com:443", "api.example.com/path"]
        {
            spec.host = Some(host.into());
            assert!(spec.validate().is_err());
        }
        spec.host = Some("api.example.com".into());
        spec.path_prefix = Some("/v1".into());
        assert!(spec.validate().is_ok());
        for path in ["v1", "/v1/../private", "/v1//private", "/v1/%2e", "/v1?key=value"] {
            spec.path_prefix = Some(path.into());
            assert!(spec.validate().is_err());
        }
        let workspace = Scope::Workspace("daily".into());
        assert!(workspace.includes(Some("daily"), None));
        assert!(!workspace.includes(None, None));
        let project = Scope::Project("/repo".into());
        assert!(project.includes(None, Some("/repo")));
        assert!(!project.includes(None, Some("/repo-other")));
        assert_eq!(service("ANTHROPIC_API_KEY"), Some(("api.anthropic.com", Auth::Header("x-api-key".into()))));
        assert!(service("EXAMPLE_UNKNOWN_KEY").is_none());
        let tool = crate::ops::SettingsTool::AddSecret { spec, value: "private-canary".into() };
        assert!(!format!("{tool:?}").contains("private-canary"));
        assert!(
            serde_json::from_str::<crate::ops::SettingsTool>(r#"{"tool":"copy_secret","name":"EXAMPLE_KEY"}"#).is_err()
        );
    }
}
