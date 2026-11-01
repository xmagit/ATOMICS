//! One OS-keychain item holds the credential catalog. Only fixed identifiers, validated hosts and
//! status leave this module; malformed entries are counted, never quoted. No plaintext fallback.
//! Besides relay credentials it holds MCP servers' environments (`mcp-env:<server>`), stored with
//! the user's consent when a connection is imported and read by `atomics mcp-host` as sbx starts it.
use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::relay::{Authorization, Credential, Keychain, Rule};

pub const KINDS: [(&str, &str, &str); 6] = [
    ("claude", "Claude subscription", "OS keychain → ATOMICS relay → api.anthropic.com (inference)"),
    ("openai", "OpenAI", "OS keychain → ATOMICS relay → api.openai.com (inference); sandbox Codex uses sbx OpenAI"),
    ("linear", "Linear", "OS keychain → ATOMICS relay → api.linear.app (GraphQL)"),
    ("github", "GitHub", "OS keychain → ATOMICS relay → api.github.com (GET/HEAD only; use a read-only token)"),
    ("registry", "Private registry", "OS keychain → ATOMICS relay → configured host (GET/HEAD only)"),
    (
        GITHUB_APP,
        "GitHub app sign-in",
        "OS keychain → ATOMICS GitHub controls on this machine (Connect GitHub; never relayed to agents)",
    ),
];

/// The catalog entry of the token ATOMICS's own GitHub controls use: `github-app` for github.com,
/// `github-app:<host>` for a GitHub Enterprise host. Unlike `github`, it is never a relay credential.
pub const GITHUB_APP: &str = "github-app";

/// The catalog id of the GitHub controls' token for `host`.
pub fn github_app_id(host: &str) -> String {
    if host.is_empty() || host == atomics_core::github::DEFAULT_HOST {
        GITHUB_APP.into()
    } else {
        format!("{GITHUB_APP}:{host}")
    }
}

// Never Debug: this structure contains real values, including untrusted entry names.
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    #[serde(default)]
    entries: BTreeMap<String, Entry>,
}

#[derive(Default, Serialize, Deserialize)]
struct Entry {
    token: String,
    #[serde(default)]
    host: Option<String>,
    /// When the token stops working, in Unix seconds; only on `github-app` entries, for an app
    /// that kept token expiration on. ATOMICS never refreshes a token: that needs the app's secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires: Option<u64>,
}

/// A GitHub controls token as stored, with when it expires (Unix seconds) if it does.
/// Never Debug: it holds the real value.
#[derive(Clone, PartialEq)]
pub struct AppGrant {
    pub token: String,
    pub expires: Option<u64>,
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
                catalog.entries.insert("claude".into(), Entry { token, ..Entry::default() });
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
            if let Some(host) = id.strip_prefix("github-app:")
                && valid_entry(id, entry)
            {
                entries.push(Status {
                    id: id.clone(),
                    label: format!("GitHub app sign-in ({host})"),
                    route: format!("OS keychain → ATOMICS GitHub controls → {host} (never relayed to agents)"),
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
        let (id, host) = match id {
            "registry" => (format!("registry:{}", host.unwrap_or_default()), host),
            // The host is part of a GitHub sign-in's id, never stored beside it.
            GITHUB_APP => (github_app_id(host.map(str::trim).unwrap_or_default()), None),
            _ => (id.into(), host),
        };
        let entry = Entry { token: token.to_string(), host: host.map(str::to_string), ..Entry::default() };
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
        let entry = Entry { token: serde_json::to_string(env)?, ..Entry::default() };
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

    /// The token ATOMICS's GitHub controls use on `host`, if one is stored.
    #[cfg(test)]
    pub fn github_app_token(&self, host: &str) -> Result<Option<String>> {
        self.token(&github_app_id(host))
    }

    /// Stores the GitHub controls' token for `host`, replacing any earlier one.
    pub fn put_github_app(&self, host: &str, token: &str) -> Result<()> {
        self.put(GITHUB_APP, token, Some(host))
    }

    /// The GitHub controls' token for `host` with its expiry, if one is stored.
    pub fn github_app_grant(&self, host: &str) -> Result<Option<AppGrant>> {
        let id = github_app_id(host);
        Ok(self
            .load()?
            .entries
            .remove(&id)
            .filter(|entry| valid_entry(&id, entry))
            .map(|entry| AppGrant { token: entry.token, expires: entry.expires }))
    }

    /// Stores a token with its expiry, as the device flow returns it, replacing any earlier one.
    pub fn put_github_app_grant(&self, host: &str, grant: &AppGrant) -> Result<()> {
        let id = github_app_id(host.trim());
        let entry = Entry { token: grant.token.clone(), host: None, expires: grant.expires };
        if !valid_entry(&id, &entry) {
            bail!("credential does not match the selected service or host (value hidden)");
        }
        let _lock = self.keychain.catalog_lock()?;
        let mut catalog = self.load()?;
        catalog.entries.insert(id, entry);
        self.save(&catalog)
    }

    /// Removes the GitHub controls' token for `host` only while it is still `token`, so a token
    /// that failed never removes one a new Connect GitHub stored meanwhile.
    pub fn remove_github_app_token(&self, host: &str, token: &str) -> Result<()> {
        let id = github_app_id(host);
        let _lock = self.keychain.catalog_lock()?;
        let mut catalog = self.load()?;
        if catalog.entries.get(&id).is_none_or(|entry| entry.token != token) {
            return Ok(());
        }
        catalog.entries.remove(&id);
        self.save(&catalog)
    }

    /// Removes the GitHub controls' token for `host`; nothing stored is fine.
    pub fn remove_github_app(&self, host: &str) -> Result<()> {
        let id = github_app_id(host);
        if !self.load()?.entries.contains_key(&id) {
            return Ok(());
        }
        self.remove(&id)
    }

    /// The JSON text of MCP OAuth item `slot`, if any. Reads take no lock: a head whose chunks were
    /// replaced while they were read fails its digest, and the read starts over from the new head.
    pub fn oauth_item(&self, slot: &str) -> Result<Option<String>> {
        check_slot(slot)?;
        let invalid = || anyhow::anyhow!("OS credential item is malformed (contents hidden); sign in again");
        for _ in 0..3 {
            let Some(head) = self.keychain.read_item(MCP_OAUTH, slot)? else { return Ok(None) };
            let encoded = match Chunks::parse(&head) {
                // Inline, as every item written before chunking is.
                None => head,
                Some(chunks) => match self.read_chunks(slot, &chunks)? {
                    Some(encoded) => encoded,
                    None => continue,
                },
            };
            if encoded.len() > OAUTH_CHUNK * OAUTH_MAX_CHUNKS || !encoded.len().is_multiple_of(2) {
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
            return String::from_utf8(bytes).map(Some).map_err(|_| invalid());
        }
        Err(invalid())
    }

    /// A digest of only the stored head, cheap even for chunked tokens. Readers compare both
    /// token and access heads so rotation without a new refresh token is detected too.
    pub(crate) fn oauth_fingerprint(&self, slot: &str) -> Result<Option<String>> {
        check_slot(slot)?;
        Ok(self.keychain.read_item(MCP_OAUTH, slot)?.map(|head| digest_hex(&head)))
    }

    /// The value `chunks` names, or `None` when a chunk is gone or the digest does not match: a writer
    /// replaced them meanwhile.
    fn read_chunks(&self, slot: &str, chunks: &Chunks) -> Result<Option<String>> {
        let mut encoded = String::new();
        for index in 0..chunks.count {
            let Some(piece) = self.keychain.read_item(MCP_OAUTH, &chunk_name(slot, chunks.generation, index))? else {
                return Ok(None);
            };
            encoded.push_str(&piece);
        }
        Ok((digest_hex(&encoded) == chunks.digest).then_some(encoded))
    }

    /// Stores `json` as MCP OAuth item `slot`, replacing any earlier one. A value too large for one item
    /// is written as chunks of the generation the head does not name, then the head is switched to
    /// them, then the previous generation is deleted: until the head is written, readers see the old
    /// value whole, and afterwards the new one.
    pub fn put_oauth_item(&self, slot: &str, json: &str) -> Result<()> {
        use std::fmt::Write;
        check_slot(slot)?;
        let mut encoded = String::new();
        for byte in json.bytes() {
            write!(encoded, "{byte:02x}")?;
        }
        if encoded.len() > OAUTH_CHUNK * OAUTH_MAX_CHUNKS {
            bail!("sign-in item is too large to store (value hidden)");
        }
        let _lock = self.keychain.catalog_lock()?;
        let previous = self.keychain.read_item(MCP_OAUTH, slot)?.as_deref().and_then(Chunks::parse);
        if encoded.len() <= OAUTH_CHUNK {
            self.keychain.write_item(MCP_OAUTH, slot, OAUTH_LABEL, &encoded)?;
        } else {
            let generation = if previous.as_ref().is_some_and(|p| p.generation == 'a') { 'b' } else { 'a' };
            let pieces: Vec<&str> =
                encoded.as_bytes().chunks(OAUTH_CHUNK).map(|piece| std::str::from_utf8(piece).unwrap()).collect();
            for (index, piece) in pieces.iter().enumerate() {
                self.keychain.write_item(MCP_OAUTH, &chunk_name(slot, generation, index), OAUTH_LABEL, piece)?;
            }
            // Longer leftovers of a write that stopped before its head.
            self.remove_chunks(slot, generation, pieces.len(), None)?;
            let head = Chunks { generation, count: pieces.len(), digest: digest_hex(&encoded) };
            self.keychain.write_item(MCP_OAUTH, slot, OAUTH_LABEL, &head.to_string())?;
        }
        // The head no longer names the previous chunks; a chunked write always used the other generation.
        if let Some(previous) = previous {
            self.remove_chunks(slot, previous.generation, 0, Some(previous.count))?;
        }
        Ok(())
    }

    /// Removes MCP OAuth item `slot` and its chunks; nothing stored is fine. The head goes first, so
    /// no reader finds a sign-in that is half gone.
    pub fn remove_oauth_item(&self, slot: &str) -> Result<()> {
        check_slot(slot)?;
        let _lock = self.keychain.catalog_lock()?;
        let previous = self.keychain.read_item(MCP_OAUTH, slot)?.as_deref().and_then(Chunks::parse);
        self.keychain.remove_item(MCP_OAUTH, slot)?;
        for generation in ['a', 'b'] {
            let count = previous.as_ref().filter(|head| head.generation == generation).map(|head| head.count);
            self.remove_chunks(slot, generation, 0, count)?;
        }
        Ok(())
    }

    /// Uses the previous head's count without reading each chunk. Only a generation with no
    /// readable head (an interrupted write) needs a bounded probe for its unknown tail.
    /// Removal runs backwards, preserving a contiguous prefix if it is interrupted.
    fn remove_chunks(&self, slot: &str, generation: char, from: usize, count: Option<usize>) -> Result<()> {
        let end = match count {
            Some(count) => count,
            None => {
                let mut end = from;
                while end < OAUTH_MAX_CHUNKS
                    && self.keychain.read_item(MCP_OAUTH, &chunk_name(slot, generation, end))?.is_some()
                {
                    end += 1;
                }
                end
            }
        };
        for index in (from..end).rev() {
            self.keychain.remove_item(MCP_OAUTH, &chunk_name(slot, generation, index))?;
        }
        Ok(())
    }

    /// Holds off every other ATOMICS process's sign-in, refresh and sign-out of the server `key`
    /// names until the returned file is dropped.
    pub fn oauth_lock(&self, key: &str) -> Result<std::fs::File> {
        check_slot(key)?;
        self.keychain.sign_in_lock(key)
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        if !KINDS.iter().any(|kind| kind.0 == id)
            && !id.strip_prefix("registry:").is_some_and(valid_host)
            && !id.strip_prefix("github-app:").is_some_and(valid_host)
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
            // ATOMICS's own GitHub controls: never handed to an agent.
            _ if id == GITHUB_APP || id.starts_with("github-app:") => return Ok(None),
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

/// The keychain service of MCP OAuth sign-in items. Each slot (a kind and a digest of the server or
/// authorization server it belongs to) is an item of its own instead of an entry in the shared
/// catalog: a few signed-in servers would crowd out the catalog's other credentials, every token
/// refresh would rewrite all of them, and signing out of one server would have to edit the others.
/// The value is hex-encoded JSON, as in the catalog.
///
/// Keychains limit one item: macOS's security(1) reads at most 4 KiB per command line, and Windows'
/// Credential Manager holds 2560 bytes per credential. A JWT access token alone can pass 2 KiB, so a
/// value longer than `OAUTH_CHUNK` hex characters is split into chunk items `<slot>-<generation><index>`
/// and the slot's own item, the head, becomes `chunks.<generation>.<count>.<sha256 of the value>`.
/// Values reach the keychain tool on standard input on every backend, never on its command line.
const MCP_OAUTH: &str = "atomics-mcp-oauth-v1";
const OAUTH_LABEL: &str = "ATOMICS MCP sign-in";
/// Hex characters per item: 1 KiB of JSON, under every backend's limit with room for the command.
const OAUTH_CHUNK: usize = 2048;
/// 16 KiB of JSON per slot.
const OAUTH_MAX_CHUNKS: usize = 16;

/// A head that names chunks. Generations alternate between `a` and `b`, so a write never touches the
/// chunks the current head names.
#[derive(Debug, PartialEq)]
struct Chunks {
    generation: char,
    count: usize,
    digest: String,
}

impl Chunks {
    /// `None` for an inline value: hex has no `.`.
    fn parse(head: &str) -> Option<Self> {
        let mut parts = head.split('.');
        let (Some("chunks"), Some(generation), Some(count), Some(digest), None) =
            (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return None;
        };
        let generation = match generation {
            "a" => 'a',
            "b" => 'b',
            _ => return None,
        };
        let count = count.parse().ok().filter(|count| (2..=OAUTH_MAX_CHUNKS).contains(count))?;
        let digest = (digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())).then(|| digest.into())?;
        Some(Self { generation, count, digest })
    }
}

impl std::fmt::Display for Chunks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "chunks.{}.{}.{}", self.generation, self.count, self.digest)
    }
}

fn chunk_name(slot: &str, generation: char, index: usize) -> String {
    format!("{slot}-{generation}{index}")
}

fn digest_hex(text: &str) -> String {
    use std::fmt::Write;
    ring::digest::digest(&ring::digest::SHA256, text.as_bytes()).as_ref().iter().fold(String::new(), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    })
}

fn check_slot(slot: &str) -> Result<()> {
    if slot.is_empty() || slot.len() > 100 || !slot.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        bail!("not a sign-in item name");
    }
    Ok(())
}

/// The catalog prefix of an MCP server's stored environment.
const MCP_ENV: &str = "mcp-env:";

fn valid_entry(id: &str, entry: &Entry) -> bool {
    let token = &entry.token;
    let github_app = id == GITHUB_APP || id.strip_prefix("github-app:").is_some_and(valid_host);
    if entry.expires.is_some() && !github_app {
        return false;
    }
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
        _ if github_app => ["gho_", "ghp_", "ghu_", "github_pat_"].iter().any(|prefix| token.starts_with(prefix)),
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
            // Every relay credential stored above shows present; the GitHub controls' token is not stored here.
            assert!(
                store
                    .snapshot()
                    .unwrap()
                    .entries
                    .iter()
                    .filter(|entry| entry.id != GITHUB_APP)
                    .all(|entry| entry.present)
            );
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
                catalog.entries.insert(id.into(), Entry { token: value.into(), ..Entry::default() });
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
    fn credentials_github_app_tokens_are_separate_and_never_relayed() {
        for mac in [true, false] {
            let (dir, _, store) = machine(mac);
            store.put("github", VALUES[3].1, None).unwrap();
            store.put_github_app("github.com", "gho_PRIVATE-app").unwrap();
            store.put(GITHUB_APP, "ghp_PRIVATE-enterprise", Some("git.example.com")).unwrap();
            assert_eq!(store.github_app_token("github.com").unwrap().as_deref(), Some("gho_PRIVATE-app"));
            assert_eq!(store.github_app_token("git.example.com").unwrap().as_deref(), Some("ghp_PRIVATE-enterprise"));
            assert_eq!(store.token("github").unwrap().as_deref(), Some(VALUES[3].1), "the agents' entry is untouched");
            assert!(store.credential(GITHUB_APP).unwrap().is_none());
            assert!(store.credential("github-app:git.example.com").unwrap().is_none());
            let snapshot = store.snapshot().unwrap();
            assert_eq!(snapshot.invalid, 0);
            assert!(snapshot.entries.iter().any(|e| e.id == GITHUB_APP && e.present));
            assert!(snapshot.entries.iter().any(|e| e.label == "GitHub app sign-in (git.example.com)" && e.present));
            let shown = format!("{snapshot:?} {}", doctor_lines(&store).join("\n"));
            let argv = std::fs::read_to_string(dir.path().join("keychain-argv")).unwrap();
            assert!(!shown.contains("PRIVATE") && !argv.contains("PRIVATE"), "{shown}");
            assert!(store.put_github_app("github.com", "lin_api_misfiled").is_err());
            assert!(store.put_github_app("127.0.0.1", "gho_value").is_err());
            let grant = AppGrant { token: "ghu_PRIVATE-user".into(), expires: Some(1_000) };
            store.put_github_app_grant("git.example.com", &grant).unwrap();
            assert!(store.github_app_grant("git.example.com").unwrap() == Some(grant.clone()));
            assert_eq!(store.github_app_token("git.example.com").unwrap().as_deref(), Some("ghu_PRIVATE-user"));
            let shown = format!("{:?} {}", store.snapshot().unwrap(), doctor_lines(&store).join("\n"));
            let argv = std::fs::read_to_string(dir.path().join("keychain-argv")).unwrap();
            assert!(!shown.contains("PRIVATE") && !argv.contains("PRIVATE"), "{shown}");
            assert!(store.snapshot().unwrap().invalid == 0);
            // A failed token is removed only while it is still the stored one.
            store.remove_github_app_token("git.example.com", "ghu_PRIVATE-other").unwrap();
            assert!(store.github_app_grant("git.example.com").unwrap().is_some());
            store.remove_github_app_token("git.example.com", "ghu_PRIVATE-user").unwrap();
            assert!(store.github_app_grant("git.example.com").unwrap().is_none());
            store.put_github_app_grant("git.example.com", &grant).unwrap();
            // Expiry belongs to the GitHub controls' entries only.
            let mut catalog = store.load().unwrap();
            catalog
                .entries
                .insert("github".into(), Entry { token: VALUES[3].1.into(), expires: Some(1), ..Entry::default() });
            store.save(&catalog).unwrap();
            assert!(store.token("github").unwrap().is_none() && store.snapshot().unwrap().invalid == 1);
            store.remove_invalid().unwrap();
            store.remove_github_app("git.example.com").unwrap();
            store.remove_github_app("git.example.com").unwrap();
            assert!(store.github_app_token("git.example.com").unwrap().is_none());
            assert!(store.github_app_token("github.com").unwrap().is_some());
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

    #[test]
    fn mcp_oauth_items_stay_outside_the_catalog_and_the_relay_and_their_values_stay_hidden() {
        for mac in [true, false] {
            let (dir, _, store) = machine(mac);
            let secret = r#"{"refresh_token":"PRIVATE-oauth-refresh"}"#;
            store.put_oauth_item("tokens-0123abcd", secret).unwrap();
            assert_eq!(store.oauth_item("tokens-0123abcd").unwrap().as_deref(), Some(secret));
            // Not a catalog entry: no snapshot line, nothing the relay could offer a session.
            let snapshot = store.snapshot().unwrap();
            assert_eq!(snapshot.invalid, 0);
            let lines = doctor_lines(&store).join("\n");
            assert!(!lines.contains("PRIVATE") && !lines.contains("oauth"), "{lines}");
            assert!(store.credential("mcp-oauth:0123abcd").unwrap().is_none());
            assert!(store.remove("mcp-oauth:0123abcd").is_err());
            // Item names are fixed-charset: nothing untrusted becomes a keychain name.
            for bad in ["", "a b", "a/../b", "tokens;rm", &"x".repeat(101)] {
                assert!(store.put_oauth_item(bad, secret).is_err() && store.oauth_item(bad).is_err());
            }
            let argv = std::fs::read_to_string(dir.path().join("keychain-argv")).unwrap();
            assert!(!argv.contains("PRIVATE"));
            store.remove_oauth_item("tokens-0123abcd").unwrap();
            assert!(store.oauth_item("tokens-0123abcd").unwrap().is_none());
        }
    }

    /// The fake keychain's MCP OAuth items: the head and every chunk of `slot`.
    fn oauth_files(dir: &Path, slot: &str) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(&format!("keychain-oauth-{slot}")))
            .collect();
        names.sort();
        names
    }

    /// A token set of `bytes` bytes of JSON, as an authorization server with long JWTs returns.
    fn token_set(bytes: usize, mark: &str) -> String {
        let fill = bytes.saturating_sub(40 + mark.len());
        format!(r#"{{"refresh_token":"PRIVATE-{mark}-{}"}}"#, "j".repeat(fill))
    }

    #[test]
    fn credentials_oauth_items_of_8_kb_round_trip_in_chunks_within_every_keychain_limit() {
        for mac in [true, false] {
            let (dir, _, store) = machine(mac);
            let slot = "tokens-0123abcd";
            let value = token_set(8 * 1024, "eight");
            store.put_oauth_item(slot, &value).unwrap();
            assert_eq!(store.oauth_item(slot).unwrap().as_deref(), Some(value.as_str()));
            // The head and 8 chunks of 1 KiB, each a write macOS's security accepts.
            let files = oauth_files(dir.path(), slot);
            assert_eq!(files.len(), 9, "{files:?}");
            let head = std::fs::read_to_string(dir.path().join(format!("keychain-oauth-{slot}"))).unwrap();
            assert!(head.starts_with("chunks.a.8."), "{head}");
            let argv = std::fs::read_to_string(dir.path().join("keychain-argv")).unwrap();
            assert!(!argv.contains("PRIVATE") && !argv.contains("6a6a6a"), "values stay off command lines");
            // A larger value than any slot holds is refused before anything changes.
            assert!(store.put_oauth_item(slot, &token_set(17 * 1024, "huge")).is_err());
            assert_eq!(store.oauth_item(slot).unwrap().as_deref(), Some(value.as_str()));
        }
    }

    #[test]
    fn credentials_oauth_items_that_shrink_leave_no_stale_chunks_and_sign_out_removes_all() {
        for mac in [true, false] {
            let (dir, _, store) = machine(mac);
            let slot = "access-0123abcd";
            let (three, two, one) = (token_set(2500, "three"), token_set(1500, "two"), token_set(300, "one"));
            store.put_oauth_item(slot, &three).unwrap();
            assert_eq!(oauth_files(dir.path(), slot).len(), 4);
            store.put_oauth_item(slot, &two).unwrap();
            assert_eq!(
                oauth_files(dir.path(), slot),
                [
                    format!("keychain-oauth-{slot}"),
                    format!("keychain-oauth-{slot}-b0"),
                    format!("keychain-oauth-{slot}-b1")
                ]
            );
            assert_eq!(store.oauth_item(slot).unwrap().as_deref(), Some(two.as_str()));
            store.put_oauth_item(slot, &three).unwrap();
            store.put_oauth_item(slot, &one).unwrap();
            assert_eq!(oauth_files(dir.path(), slot), [format!("keychain-oauth-{slot}")], "3 chunks became 1 item");
            assert_eq!(store.oauth_item(slot).unwrap().as_deref(), Some(one.as_str()));

            // A write that stopped after its chunks, before its head: the old value still reads, and
            // the next write and the sign-out sweep its leftovers.
            store.put_oauth_item(slot, &three).unwrap();
            for index in 0..5 {
                std::fs::write(dir.path().join(format!("keychain-oauth-{slot}-b{index}")), "00").unwrap();
            }
            assert_eq!(store.oauth_item(slot).unwrap().as_deref(), Some(three.as_str()));
            store.put_oauth_item(slot, &two).unwrap();
            assert_eq!(oauth_files(dir.path(), slot).len(), 3, "{:?}", oauth_files(dir.path(), slot));
            assert_eq!(store.oauth_item(slot).unwrap().as_deref(), Some(two.as_str()));
            std::fs::write(dir.path().join(format!("keychain-oauth-{slot}-a0")), "00").unwrap();

            store.remove_oauth_item(slot).unwrap();
            assert!(oauth_files(dir.path(), slot).is_empty(), "{:?}", oauth_files(dir.path(), slot));
            assert!(store.oauth_item(slot).unwrap().is_none());
        }
    }

    #[test]
    fn credentials_oauth_cleanup_uses_the_previous_heads_count() {
        for mac in [true, false] {
            let (dir, _, store) = machine(mac);
            let slot = "tokens-count";
            store.put_oauth_item(slot, &token_set(8 * 1024, "eight")).unwrap();
            let argv = dir.path().join("keychain-argv");
            std::fs::write(&argv, "").unwrap();
            store.put_oauth_item(slot, "{}").unwrap();
            let calls = std::fs::read_to_string(&argv).unwrap();
            let reads = calls.lines().filter(|line| matches!(*line, "lookup" | "find-generic-password")).count();
            assert_eq!(reads, 2, "only the old head and verified new head are read, not the eight old chunks");

            store.put_oauth_item(slot, &token_set(8 * 1024, "eight")).unwrap();
            std::fs::write(&argv, "").unwrap();
            store.remove_oauth_item(slot).unwrap();
            let calls = std::fs::read_to_string(&argv).unwrap();
            let reads = calls.lines().filter(|line| matches!(*line, "lookup" | "find-generic-password")).count();
            assert_eq!(reads, 2, "the head and the unnamed generation's first slot, not each named chunk");
            assert!(oauth_files(dir.path(), slot).is_empty());
        }
    }

    #[test]
    fn credentials_oauth_readers_never_take_a_mix_of_old_and_new_chunks() {
        let (dir, _, store) = machine(false);
        let slot = "tokens-0123abcd";
        let (old, new) = (token_set(2500, "old"), token_set(2500, "new"));
        store.put_oauth_item(slot, &old).unwrap();
        // A chunk of another value under the head's names: the digest refuses the mix, never returns it.
        let foreign = std::fs::read_to_string(dir.path().join(format!("keychain-oauth-{slot}-a0"))).unwrap();
        store.put_oauth_item(slot, &new).unwrap();
        store.put_oauth_item(slot, &new).unwrap();
        std::fs::write(dir.path().join(format!("keychain-oauth-{slot}-a0")), foreign).unwrap();
        let error = store.oauth_item(slot).unwrap_err().to_string();
        assert!(error.contains("malformed") && !error.contains("PRIVATE"), "{error}");
        // Old-format and inline heads still read; a head naming chunks that are not there does not.
        assert_eq!(Chunks::parse("7b7d"), None);
        assert_eq!(Chunks::parse(&format!("chunks.c.2.{}", "0".repeat(64))), None);
        assert_eq!(Chunks::parse(&format!("chunks.a.1.{}", "0".repeat(64))), None);
        let head = Chunks { generation: 'b', count: 3, digest: "0".repeat(64) };
        assert_eq!(Chunks::parse(&head.to_string()), Some(head));
    }
}
