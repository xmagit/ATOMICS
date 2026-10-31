//! One OS-keychain item holds the credential catalog. Only fixed identifiers, validated hosts and
//! status leave this module; malformed entries are counted, never quoted. No plaintext fallback.
//! Besides relay credentials it holds MCP servers' environments (`mcp-env:<server>`), stored with
//! the user's consent when a connection is imported and read by `atomics mcp-host` as sbx starts it.
use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::relay::{Authorization, Credential, Keychain, Rule};

pub const KINDS: [(&str, &str, &str); 5] = [
    ("claude", "Claude subscription", "OS keychain → ATOMICS relay → api.anthropic.com (inference)"),
    ("openai", "OpenAI", "OS keychain → ATOMICS relay → api.openai.com (inference); sandbox Codex uses sbx OpenAI"),
    ("linear", "Linear", "OS keychain → ATOMICS relay → api.linear.app (GraphQL)"),
    ("github", "GitHub", "OS keychain → ATOMICS relay → api.github.com (GET/HEAD only; use a read-only token)"),
    ("registry", "Private registry", "OS keychain → ATOMICS relay → configured host (GET/HEAD only)"),
];

// Never Debug: this structure contains real values, including untrusted entry names.
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    #[serde(default)]
    entries: BTreeMap<String, Entry>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    token: String,
    #[serde(default)]
    host: Option<String>,
}

#[derive(Clone)]
pub struct Store {
    keychain: Keychain,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub id: String,
    pub label: String,
    pub present: bool,
    pub route: String,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub entries: Vec<Status>,
    pub invalid: usize,
}

impl Snapshot {
    pub fn lines(&self) -> Vec<String> {
        let mut lines: Vec<_> = self
            .entries
            .iter()
            .map(|entry| {
                format!(
                    "{} credential {}: {}; {}",
                    if entry.present { "✓" } else { "·" },
                    entry.label,
                    if entry.present { "present" } else { "absent" },
                    entry.route
                )
            })
            .collect();
        if self.invalid > 0 {
            lines.push(format!(
                "✗ {} unknown or misfiled credential entries (names and values hidden); replace with known entries",
                self.invalid
            ));
        }
        lines
    }
}

impl Store {
    pub fn new(keychain: Keychain) -> Self {
        Self { keychain }
    }

    fn load(&self) -> Result<Catalog> {
        let Some(encoded) = self.keychain.read_catalog()? else {
            let mut catalog = Catalog::default();
            if let Some(token) = self.keychain.legacy_token() {
                catalog.entries.insert("claude".into(), Entry { token, host: None });
            }
            return Ok(catalog);
        };
        let invalid = || {
            anyhow::anyhow!(
                "OS credential catalog is malformed (contents hidden); repair it privately in the OS keychain"
            )
        };
        if encoded.len() > 128 * 1024 || !encoded.len().is_multiple_of(2) {
            return Err(invalid());
        }
        let bytes: Vec<u8> = encoded
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                let digit = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
                Some(digit(pair[0])? * 16 + digit(pair[1])?)
            })
            .collect::<Option<_>>()
            .ok_or_else(invalid)?;
        serde_json::from_slice(&bytes).map_err(|_| invalid())
    }

    fn save(&self, catalog: &Catalog) -> Result<()> {
        use std::fmt::Write;
        let mut encoded = String::new();
        for byte in serde_json::to_vec(catalog)? {
            write!(encoded, "{byte:02x}")?;
        }
        if encoded.len() > 128 * 1024 {
            bail!("credential catalog is full; remove unused entries first");
        }
        self.keychain.write_catalog(&encoded)?;
        self.keychain.remove_legacy()
    }

    pub fn snapshot(&self) -> Result<Snapshot> {
        let catalog = self.load()?;
        let invalid = catalog.entries.iter().filter(|(id, entry)| !valid_entry(id, entry)).count();
        let mut entries: Vec<_> = KINDS
            .iter()
            .filter(|kind| kind.0 != "registry")
            .map(|&(id, label, route)| Status {
                id: id.into(),
                label: label.into(),
                route: route.into(),
                present: catalog.entries.get(id).is_some_and(|entry| valid_entry(id, entry)),
            })
            .collect();
        for (id, entry) in &catalog.entries {
            if id.starts_with("registry:") && valid_entry(id, entry) {
                entries.push(Status {
                    id: id.clone(),
                    label: format!("Private registry ({})", entry.host.as_deref().unwrap()),
                    route: format!("OS keychain → ATOMICS relay → {} (GET/HEAD only)", entry.host.as_deref().unwrap()),
                    present: true,
                });
            }
        }
        for (id, entry) in &catalog.entries {
            if let Some(server) = id.strip_prefix(MCP_ENV)
                && valid_entry(id, entry)
            {
                entries.push(Status {
                    id: id.clone(),
                    label: format!("MCP {server} environment"),
                    route: format!("OS keychain → atomics mcp-host → {server} on this machine (values hidden)"),
                    present: true,
                });
            }
        }
        if !entries.iter().any(|entry| entry.id.starts_with("registry:")) {
            entries.push(Status {
                id: "registry".into(),
                label: "Private registry".into(),
                route: KINDS[4].2.into(),
                present: false,
            });
        }
        Ok(Snapshot { entries, invalid })
    }

    pub fn token(&self, id: &str) -> Result<Option<String>> {
        Ok(self.load()?.entries.remove(id).filter(|entry| valid_entry(id, entry)).map(|entry| entry.token))
    }

    pub fn put(&self, id: &str, token: &str, host: Option<&str>) -> Result<()> {
        let id = if id == "registry" { format!("registry:{}", host.unwrap_or_default()) } else { id.into() };
        let entry = Entry { token: token.to_string(), host: host.map(str::to_string) };
        if !valid_entry(&id, &entry) {
            bail!("credential does not match the selected service or host (value hidden)");
        }
        let _lock = self.keychain.catalog_lock()?;
        let mut catalog = self.load()?;
        catalog.entries.insert(id, entry);
        self.save(&catalog)
    }

    /// Stores the environment an MCP server's host process gets, replacing any earlier one.
    pub fn put_mcp_env(&self, server: &str, env: &BTreeMap<String, String>) -> Result<()> {
        let id = format!("{MCP_ENV}{server}");
        let entry = Entry { token: serde_json::to_string(env)?, host: None };
        if !valid_entry(&id, &entry) {
            bail!("MCP environment for {server} is not storable (values hidden)");
        }
        let _lock = self.keychain.catalog_lock()?;
        let mut catalog = self.load()?;
        catalog.entries.insert(id, entry);
        self.save(&catalog)
    }

    /// The environment stored for MCP server `server`, if any.
    pub fn mcp_env(&self, server: &str) -> Result<Option<BTreeMap<String, String>>> {
        let id = format!("{MCP_ENV}{server}");
        Ok(self
            .load()?
            .entries
            .remove(&id)
            .filter(|entry| valid_entry(&id, entry))
            .and_then(|entry| serde_json::from_str(&entry.token).ok()))
    }

    /// Removes MCP server `server`'s stored environment; nothing stored is fine.
    pub fn remove_mcp_env(&self, server: &str) -> Result<()> {
        let id = format!("{MCP_ENV}{server}");
        if !self.load()?.entries.contains_key(&id) {
            return Ok(());
        }
        self.remove(&id)
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        if !KINDS.iter().any(|kind| kind.0 == id)
            && !id.strip_prefix("registry:").is_some_and(valid_host)
            && !id.strip_prefix(MCP_ENV).is_some_and(atomics_core::mcp_import::valid_name)
        {
            bail!("unknown credential type");
        }
        let _lock = self.keychain.catalog_lock()?;
        let mut catalog = self.load()?;
        catalog.entries.remove(id);
        self.save(&catalog)
    }

    /// Explicit cleanup drops unknown/misfiled entries without exposing their names.
    pub fn remove_invalid(&self) -> Result<()> {
        let _lock = self.keychain.catalog_lock()?;
        let mut catalog = self.load()?;
        catalog.entries.retain(|id, entry| valid_entry(id, entry));
        self.save(&catalog)
    }

    pub fn credential(&self, id: &str) -> Result<Option<Credential>> {
        let Some(entry) = self.load()?.entries.remove(id).filter(|entry| valid_entry(id, entry)) else {
            return Ok(None);
        };
        let token = entry.token;
        let (label, host, rules) = match id {
            "claude" => return Ok(Some(Credential::claude(token))),
            "openai" => (
                "OpenAI",
                "api.openai.com".into(),
                vec![
                    Rule::new("api.openai.com", "POST", "/v1/responses"),
                    Rule::new("api.openai.com", "POST", "/v1/chat/completions"),
                    Rule::new("api.openai.com", "GET", "/v1/models"),
                ],
            ),
            "linear" => ("Linear", "api.linear.app".into(), vec![Rule::new("api.linear.app", "POST", "/graphql")]),
            "github" => ("GitHub", "api.github.com".into(), read_rules("api.github.com")),
            // A host process's environment, not a relay credential.
            _ if id.starts_with(MCP_ENV) => return Ok(None),
            _ if id.starts_with("registry:") => {
                let host = entry.host.expect("validated host");
                return Ok(Some(Credential::stored(
                    format!("Private registry ({host})"),
                    token,
                    vec![host.clone()],
                    read_rules(&host),
                    Authorization::Bearer,
                )));
            }
            _ => unreachable!("validated service"),
        };
        let authorization = if id == "linear" { Authorization::ApiKey } else { Authorization::Bearer };
        Ok(Some(Credential::stored(label.into(), token, vec![host], rules, authorization)))
    }
}

fn read_rules(host: &str) -> Vec<Rule> {
    ["GET", "HEAD"].into_iter().map(|method| Rule::new(host, method, "/*")).collect()
}

/// The catalog prefix of an MCP server's stored environment.
const MCP_ENV: &str = "mcp-env:";

fn valid_entry(id: &str, entry: &Entry) -> bool {
    let token = &entry.token;
    if let Some(server) = id.strip_prefix(MCP_ENV) {
        return entry.host.is_none()
            && atomics_core::mcp_import::valid_name(server)
            && token.len() <= 16 * 1024
            && serde_json::from_str::<BTreeMap<String, String>>(token).is_ok_and(|env| {
                !env.is_empty()
                    && env
                        .iter()
                        .all(|(name, value)| atomics_core::mcp_import::valid_env_name(name) && !value.contains('\0'))
            });
    }
    if token.is_empty() || token.len() > 8192 || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return false;
    }
    if !id.starts_with("registry:") && entry.host.is_some() {
        return false;
    }
    match id {
        "claude" => crate::relay::valid_claude_token(token),
        "openai" => token.starts_with("sk-") && !token.starts_with("sk-ant-"),
        "linear" => token.starts_with("lin_api_"),
        "github" => token.starts_with("github_pat_") || token.starts_with("ghp_"),
        _ if id.starts_with("registry:") => {
            entry.host.as_deref().is_some_and(|host| valid_host(host) && id.strip_prefix("registry:") == Some(host))
        }
        _ => false,
    }
}

fn valid_host(host: &str) -> bool {
    host.len() <= 253
        && host.contains('.')
        && host.parse::<std::net::IpAddr>().is_err()
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

pub fn doctor_lines(store: &Store) -> Vec<String> {
    match store.snapshot() {
        Ok(snapshot) => snapshot.lines(),
        Err(error) => vec![format!("✗ credential store (warning): {error}")],
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::Path;

    pub fn machine(mac: bool) -> (crate::test_support::TempDir, Keychain, Store) {
        let dir = crate::test_support::tempdir().unwrap();
        let program = dir.path().join("keychain");
        std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/keychain"), &program).unwrap();
        let keychain = Keychain::at(&program, mac);
        let store = Store::new(keychain.clone());
        (dir, keychain, store)
    }

    const VALUES: [(&str, &str, Option<&str>); 5] = [
        ("claude", "sk-ant-oat01-PRIVATE-subscription", None),
        ("openai", "sk-PRIVATE-openai", None),
        ("linear", "lin_api_PRIVATE-linear", None),
        ("github", "github_pat_PRIVATE-github", None),
        ("registry:registry.example.com", "PRIVATE-registry-!+/=", Some("registry.example.com")),
    ];

    #[test]
    fn credentials_keychain_reads_use_pipes_even_for_legacy_entries_and_errors() {
        for mac in [true, false] {
            let (dir, keychain, store) = machine(mac);
            std::fs::write(dir.path().join("check-output-pipes"), "").unwrap();
            std::fs::write(dir.path().join("keychain-value"), VALUES[0].1).unwrap();
            assert_eq!(keychain.legacy_token().as_deref(), Some(VALUES[0].1));
            store.put("claude", VALUES[0].1, None).unwrap();
            assert!(keychain.read_catalog().unwrap().is_some());
            assert_eq!(store.token("claude").unwrap().as_deref(), Some(VALUES[0].1));
            std::fs::write(dir.path().join("fail-keychain"), "").unwrap();
            assert!(keychain.legacy_token().is_none());
            let error = keychain.read_catalog().unwrap_err().to_string();
            assert!(!error.contains("PRIVATE"));
            let fds = std::fs::read_to_string(dir.path().join("keychain-output-fds")).unwrap();
            assert!(fds.lines().count() >= 6, "legacy/catalog/read-back/error reads were exercised");
            assert!(fds.lines().all(|line| line == "True True"), "{fds}");
        }
    }

    #[test]
    fn credentials_on_both_keychains_never_reach_argv_status_or_errors() {
        for mac in [true, false] {
            let (dir, keychain, store) = machine(mac);
            for (id, value, host) in VALUES {
                store.put(id, value, host).unwrap();
                assert_eq!(store.token(id).unwrap().as_deref(), Some(value));
                assert!(store.credential(id).unwrap().is_some());
            }
            assert_eq!(keychain.claude_token().as_deref(), Some(VALUES[0].1));
            assert!(store.snapshot().unwrap().entries.iter().all(|entry| entry.present));
            let output = format!(
                "{:?}\n{}\n{:?}",
                store.snapshot().unwrap(),
                doctor_lines(&store).join("\n"),
                store.credential("github").unwrap()
            );
            let argv = std::fs::read_to_string(dir.path().join("keychain-argv")).unwrap();
            for (_, value, _) in VALUES {
                assert!(!output.contains(value) && !argv.contains(value));
            }
            store.put("claude", "sk-ant-oat01-REPLACED", None).unwrap();
            assert_eq!(keychain.claude_token().as_deref(), Some("sk-ant-oat01-REPLACED"));
            for (id, _, _) in VALUES {
                store.remove(id).unwrap();
            }
            assert!(store.snapshot().unwrap().entries.iter().all(|entry| !entry.present));
            // A legacy token cannot resurrect a deliberately removed catalog entry.
            std::fs::write(dir.path().join("keychain-value"), VALUES[0].1).unwrap();
            assert!(keychain.claude_token().is_none());
            std::fs::write(dir.path().join("fail-keychain"), "").unwrap();
            let output = format!("{:?}\n{:?}", store.put("claude", VALUES[0].1, None), doctor_lines(&store));
            assert!(!output.contains("PRIVATE") && output.contains("unavailable"));
        }
    }

    #[test]
    fn credentials_detect_unknown_misfiled_and_malformed_without_quoting_them() {
        for mac in [true, false] {
            let (dir, _, store) = machine(mac);
            let mut catalog = Catalog::default();
            for (id, value) in [("sk-PRIVATE-name", "PRIVATE-value"), ("openai", "sk-ant-oat01-PRIVATE-misfiled")] {
                catalog.entries.insert(id.into(), Entry { token: value.into(), host: None });
            }
            store.save(&catalog).unwrap();
            let snapshot = store.snapshot().unwrap();
            assert_eq!(snapshot.invalid, 2);
            assert!(!snapshot.entries.iter().any(|entry| entry.present));
            assert!(!format!("{snapshot:?} {:?}", doctor_lines(&store)).contains("PRIVATE"));
            assert!(store.token("openai").unwrap().is_none());
            store.remove_invalid().unwrap();
            assert_eq!(store.snapshot().unwrap().invalid, 0);
            for value in ["PRIVATE-not-hex", "7b2250524956415445223a22736563726574227d"] {
                std::fs::write(dir.path().join("keychain-catalog"), value).unwrap();
                let lines = doctor_lines(&store).join("\n");
                assert!(!lines.contains("PRIVATE"));
                assert!(store.put("openai", "sk-test", None).is_err(), "never overwrite an unreadable catalog");
            }
        }
    }

    #[test]
    fn credentials_hold_mcp_environments_without_showing_or_relaying_them() {
        for mac in [true, false] {
            let (dir, _, store) = machine(mac);
            let env = BTreeMap::from([
                ("GITHUB_TOKEN".to_string(), "ghp_PRIVATE value with spaces".to_string()),
                ("MODE".to_string(), "PRIVATE-mode".to_string()),
            ]);
            store.put_mcp_env("github", &env).unwrap();
            assert_eq!(store.mcp_env("github").unwrap(), Some(env));
            assert_eq!(store.mcp_env("other").unwrap(), None);
            let snapshot = store.snapshot().unwrap();
            assert_eq!(snapshot.invalid, 0);
            let entry = snapshot.entries.iter().find(|entry| entry.id == "mcp-env:github").unwrap();
            assert!(entry.present && entry.label == "MCP github environment");
            assert!(store.credential("mcp-env:github").unwrap().is_none(), "never a relay credential");
            let shown = format!("{snapshot:?} {}", doctor_lines(&store).join("\n"));
            let argv = std::fs::read_to_string(dir.path().join("keychain-argv")).unwrap();
            assert!(!shown.contains("PRIVATE") && !argv.contains("PRIVATE"), "{shown}");
            for (server, name) in [("atomics-s1", "A"), ("Bad Name", "A"), ("github", "1BAD"), ("github", "A=B")] {
                let env = BTreeMap::from([(name.to_string(), "value".to_string())]);
                assert!(store.put_mcp_env(server, &env).is_err(), "{server} {name}");
            }
            assert!(store.put_mcp_env("github", &BTreeMap::new()).is_err());
            store.remove_mcp_env("github").unwrap();
            store.remove_mcp_env("github").unwrap();
            assert_eq!(store.mcp_env("github").unwrap(), None);
        }
    }

    #[test]
    fn credentials_saved_before_the_rename_are_read_until_the_next_save_moves_them() {
        for mac in [true, false] {
            let (dir, _, store) = machine(mac);
            store.put("claude", VALUES[0].1, None).unwrap();
            // The catalog as IMACS saved it, under its own keychain item.
            std::fs::rename(dir.path().join("keychain-catalog"), dir.path().join("keychain-catalog-imacs")).unwrap();
            assert_eq!(store.token("claude").unwrap().as_deref(), Some(VALUES[0].1));
            store.put("openai", VALUES[1].1, None).unwrap();
            assert!(dir.path().join("keychain-catalog").exists());
            assert_eq!(store.token("claude").unwrap().as_deref(), Some(VALUES[0].1));
            assert_eq!(store.token("openai").unwrap().as_deref(), Some(VALUES[1].1));
        }
    }

    #[test]
    fn credentials_reject_oversized_macos_writes_before_changing_the_keychain() {
        let (dir, keychain, store) = machine(true);
        store.put("claude", "sk-ant-oat01-ORIGINAL", None).unwrap();
        let before = std::fs::read(dir.path().join("keychain-catalog")).unwrap();
        let calls = std::fs::read_to_string(dir.path().join("keychain-argv")).unwrap();
        let error = store.put("openai", &format!("sk-{}", "x".repeat(2048)), None).unwrap_err().to_string();
        assert!(error.contains("4 KiB") && !error.contains("sk-"));
        assert_eq!(std::fs::read(dir.path().join("keychain-catalog")).unwrap(), before);
        let after = std::fs::read_to_string(dir.path().join("keychain-argv")).unwrap();
        assert_eq!(after.matches("-i").count(), calls.matches("-i").count());
        assert_eq!(keychain.claude_token().as_deref(), Some("sk-ant-oat01-ORIGINAL"));
    }

    #[test]
    fn credentials_validate_service_and_registry_host_before_writing() {
        let (dir, _, store) = machine(true);
        for (id, value, host) in [
            ("claude", "sk-ant-oat01-injected\ncommand", None),
            ("openai", "sk-ant-oat01-misfiled", None),
            ("linear", "ghp_misfiled", None),
            ("registry", "secret", Some("https://registry.example.com")),
            ("registry", "secret", Some("127.0.0.1")),
            ("registry", "secret", Some("registry.example.com/path")),
            ("PRIVATE-unknown", "secret", None),
        ] {
            assert!(store.put(id, value, host).is_err());
        }
        assert!(!dir.path().join("keychain-catalog").exists());
    }

    #[test]
    fn credentials_linear_uses_raw_api_key_authorization_and_accepts_a_raw_placeholder() {
        use crate::relay::{Service, fake};
        let (dir, keychain, store) = machine(cfg!(target_os = "macos"));
        store.put("linear", "lin_api_PRIVATE-linear", None).unwrap();
        let upstream = fake::start();
        let service = Service::with_upstreams(
            dir.path().join("home"),
            keychain,
            std::collections::HashMap::from([("api.linear.app".into(), upstream.url.clone())]),
        );
        let grant = service.grant("linear-session", "linear").unwrap().unwrap();
        upstream.release.send(()).unwrap();
        let request = format!(
            "POST /api.linear.app/graphql HTTP/1.1\r\nAuthorization: {}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}",
            grant.placeholder
        );
        let (head, mut response) = fake::send(grant.port, &request);
        assert!(head.contains(" 200 "));
        // HTTP chunk boundaries need not coincide with event boundaries (Linux commonly coalesces them).
        let mut body = String::new();
        loop {
            let chunk = fake::chunk(&mut response);
            if chunk.is_empty() {
                break;
            }
            body.push_str(&chunk);
        }
        assert!(body.contains("message_start") && body.contains("message_stop"));
        assert_eq!(upstream.seen.lock().unwrap()[0].header("authorization"), ["lin_api_PRIVATE-linear"]);
        let output = std::fs::read_to_string(dir.path().join("home/relay.log")).unwrap_or_default();
        assert!(!output.contains("lin_api_PRIVATE-linear") && !output.contains(&grant.placeholder));
    }

    #[test]
    fn credentials_relay_rules_restrict_hosts_and_capabilities_and_track_usage() {
        use crate::relay::{Service, fake};
        for mac in [true, false] {
            let (dir, keychain, store) = machine(mac);
            store.put("github", VALUES[3].1, None).unwrap();
            let fake = fake::start();
            let home = dir.path().join("home");
            let service = Service::with_upstreams(
                home.clone(),
                keychain,
                std::collections::HashMap::from([("api.github.com".into(), fake.url.clone())]),
            );
            let grant = service.grant("bare-session", "github").unwrap().unwrap();
            let request = |host, method, path| {
                let request = format!(
                    "{method} /{host}{path} HTTP/1.1\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
                    grant.placeholder
                );
                fake::send(grant.port, &request).0
            };
            assert!(request("api.github.com", "POST", "/repos/owner/repo").contains(" 403 "));
            assert!(request("evil.example", "GET", "/v1/models").contains(" 403 "));
            assert!(request("api.github.com", "GET", "/v1/models").contains(" 200 "));
            let seen = fake.seen.lock().unwrap();
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0].header("authorization"), [format!("Bearer {}", VALUES[3].1)]);
            drop(seen);
            let (last, sessions) = service.usage("GitHub");
            assert!(last.is_some());
            assert_eq!(sessions, ["bare-session"]);
            service.revoke_credential("GitHub");
            assert!(request("api.github.com", "GET", "/v1/models").contains(" 401 "));
            assert!(service.usage("GitHub").1.is_empty());
            let output = format!(
                "{}\n{}\n{:?}",
                std::fs::read_to_string(home.join("relay.log")).unwrap(),
                std::fs::read_to_string(home.join("credential-usage.json")).unwrap(),
                grant
            );
            assert!(!output.contains(VALUES[3].1) && !output.contains(&grant.placeholder));
        }
    }
}
