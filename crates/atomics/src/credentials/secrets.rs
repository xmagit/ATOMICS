//! General secrets live in the existing catalog. Only metadata may leave management APIs.
use super::*;
use anyhow::Context;
use atomics_core::secrets::Secret;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Stored {
    pub spec: Secret,
    value: String,
}

impl Stored {
    fn valid(&self, name: &str) -> bool {
        self.spec.name == name && self.spec.validate().is_ok() && valid_value(&self.spec, &self.value)
    }
}

fn valid_value(spec: &Secret, value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 8192
        && !value.contains('\0')
        && (spec.host.is_none() || value.bytes().all(|b| b.is_ascii() && !b.is_ascii_control()))
}

impl Store {
    pub fn connect_github_account(&self, account: &str, token: &str) -> Result<()> {
        let mut spec = crate::relay::github::account_spec(account)?;
        if let Some(existing) = self.secrets()?.into_iter().find(|s| s.name == spec.name) {
            spec.scope = existing.scope;
            spec.refuse_default_branch = existing.refuse_default_branch;
        }
        if let Some(client) = &self.broker {
            return client.set(spec, token);
        }
        self.sync_secret_rule(spec, token)?;
        Ok(())
    }

    pub fn set_github_policy(&self, name: &str, refuse_default_branch: bool) -> Result<()> {
        anyhow::ensure!(name.starts_with(crate::relay::github::ACCOUNT_PREFIX), "Not a GitHub account secret");
        if let Some(client) = &self.broker {
            return client.github_policy(name, refuse_default_branch);
        }
        let (mut spec, value) =
            self.secret_values()?.into_iter().find(|(s, _)| s.name == name).context("Unknown GitHub account")?;
        spec.refuse_default_branch = refuse_default_branch;
        self.sync_secret_rule(spec, &value)?;
        Ok(())
    }

    pub fn secrets(&self) -> Result<Vec<Secret>> {
        if let Some(client) = &self.broker {
            return client.list();
        }
        let catalog = self.load()?;
        if catalog.secrets.iter().any(|(name, entry)| !entry.valid(name)) {
            bail!("Secret metadata is malformed (contents hidden); repair it privately in the credential store");
        }
        Ok(catalog.secrets.into_values().map(|entry| entry.spec).collect())
    }

    pub fn add_secret(&self, mut spec: Secret, value: &str) -> Result<()> {
        if let atomics_core::secrets::Scope::Project(path) = &mut spec.scope {
            *path = atomics_core::workspace::project_root(std::path::Path::new(path))
                .map_err(|_| anyhow::anyhow!("Project scope must name an existing git checkout"))?
                .to_string_lossy()
                .into_owned();
        }
        self.add_secret_rule(spec, value)
    }

    /// The broker accepts already canonicalized metadata from its administrator without opening
    /// agent-owned project directories. Secret::validate still checks every rule field.
    pub(crate) fn add_secret_rule(&self, spec: Secret, value: &str) -> Result<()> {
        spec.validate().map_err(anyhow::Error::msg)?;
        if !valid_value(&spec, value) {
            bail!("Secret value is empty, too large or contains unsupported characters (value hidden)");
        }
        if let Some(client) = &self.broker {
            return client.set(spec, value);
        }
        let _lock = self.keychain.catalog_lock()?;
        let mut catalog = self.load()?;
        if catalog.secrets.contains_key(&spec.name) {
            bail!("That secret name already exists; remove it before adding a new value");
        }
        catalog.secrets.insert(spec.name.clone(), Stored { spec, value: value.into() });
        self.save(&catalog)
    }

    pub fn remove_secret(&self, name: &str) -> Result<()> {
        if let Some(client) = &self.broker {
            return client.remove(name);
        }
        if !atomics_core::secrets::valid_name(name) {
            bail!("Invalid secret name");
        }
        let _lock = self.keychain.catalog_lock()?;
        let mut catalog = self.load()?;
        if catalog.secrets.remove(name).is_none() {
            bail!("Secret does not exist");
        }
        self.save(&catalog)
    }

    /// Administrator sync is idempotent. An unchanged rule/value keeps live grants; a changed
    /// entry is replaced atomically, and the broker revokes grants made from its old value.
    pub(crate) fn sync_secret_rule(&self, spec: Secret, value: &str) -> Result<bool> {
        spec.validate().map_err(anyhow::Error::msg)?;
        if !valid_value(&spec, value) || self.broker.is_some() {
            bail!("Invalid private secret sync");
        }
        let _lock = self.keychain.catalog_lock()?;
        let mut catalog = self.load()?;
        if catalog.secrets.get(&spec.name).is_some_and(|old| old.spec == spec && old.value == value) {
            return Ok(false);
        }
        catalog.secrets.insert(spec.name.clone(), Stored { spec, value: value.into() });
        self.save(&catalog)?;
        Ok(true)
    }

    /// Only session preparation and the authenticated native clipboard worker read values.
    pub(crate) fn secret_values(&self) -> Result<Vec<(Secret, Zeroizing<String>)>> {
        if self.broker.is_some() {
            bail!("Broker secrets cannot be read or copied; only exposed session values may leave it");
        }
        let catalog = self.load()?;
        if catalog.secrets.iter().any(|(name, entry)| !entry.valid(name)) {
            bail!("Secret catalog is malformed (contents hidden)");
        }
        Ok(catalog.secrets.into_values().map(|entry| (entry.spec, Zeroizing::new(entry.value))).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomics_core::secrets::{Auth, Scope};

    #[test]
    fn secrets_canary_stays_in_keychain_and_exposed_session_environment() {
        for mac in [true, false] {
            let (dir, keychain, store) = super::super::tests::machine(mac);
            let relayed = format!("canary-relayed-{}", uuid::Uuid::new_v4().simple());
            let exposed = format!("canary-exposed-{}", uuid::Uuid::new_v4().simple());
            store
                .add_secret(
                    Secret {
                        name: "EXAMPLE_KEY".into(),
                        host: Some("api.example.com".into()),
                        scope: Scope::Workspace("daily".into()),
                        ..Default::default()
                    },
                    &relayed,
                )
                .unwrap();
            store.add_secret(Secret { name: "EXPOSED_KEY".into(), ..Default::default() }, &exposed).unwrap();
            let relay = crate::relay::Service::new(dir.path().join("state"), keychain);
            let grant = relay.grant_secrets("s1", Some("daily"), None, false, None).unwrap().unwrap();
            assert_eq!(grant.secrets["EXPOSED_KEY"], exposed);
            assert!(grant.secrets["EXAMPLE_KEY"].starts_with("atomics-"));
            assert_ne!(grant.secrets["EXAMPLE_KEY"], relayed);
            assert!(grant.secrets["EXAMPLE_KEY_RELAY_URL"].ends_with("/api.example.com"));
            let elsewhere = relay.grant_secrets("s2", Some("other"), None, false, None).unwrap().unwrap();
            assert!(!elsewhere.secrets.contains_key("EXAMPLE_KEY"));
            let status = format!("{:?} {:?} {:?}", store.secrets().unwrap(), grant, elsewhere);
            for canary in [relayed.as_str(), exposed.as_str()] {
                assert!(!status.contains(canary));
            }
            let entries = std::fs::read_dir(dir.path()).unwrap();
            for path in entries.map(|entry| entry.unwrap().path()).filter(|p| p.is_file()) {
                if path.file_name().unwrap() == "keychain-catalog" {
                    continue;
                }
                let bytes = std::fs::read(path).unwrap();
                for canary in [relayed.as_str(), exposed.as_str()] {
                    assert!(!bytes.windows(canary.len()).any(|w| w == canary.as_bytes()));
                }
            }
            let grant_for_removal = grant.secrets["EXAMPLE_KEY"].clone();
            relay.edit_removal("secret:EXAMPLE_KEY", |store| store.remove_secret("EXAMPLE_KEY")).unwrap();
            assert_eq!(relay.live_grants(), 0);
            assert!(!grant_for_removal.is_empty());
            relay.revoke(&grant);
            relay.revoke(&elsewhere);
        }
    }

    #[test]
    fn general_secrets_preserve_existing_catalog_entries_and_validate_metadata() {
        let (_dir, _, store) = super::super::tests::machine(false);
        store.put("openai", "sk-canary-existing", None).unwrap();
        store
            .add_secret(
                Secret {
                    name: "EXAMPLE_KEY".into(),
                    auth: Auth::Query("key".into()),
                    host: Some("api.example.com".into()),
                    ..Default::default()
                },
                "canary-new",
            )
            .unwrap();
        assert!(store.add_secret(Secret { name: "EXAMPLE_KEY".into(), ..Default::default() }, "replacement").is_err());
        store.remove_secret("EXAMPLE_KEY").unwrap();
        assert_eq!(store.token("openai").unwrap().as_deref(), Some("sk-canary-existing"));
        assert!(store.secrets().unwrap().is_empty());
    }

    #[test]
    fn scoped_model_secrets_override_legacy_settings_without_serializing_values() {
        let (dir, keychain, store) = super::super::tests::machine(false);
        store.put("claude", "sk-ant-oat01-canary-legacy", None).unwrap();
        store
            .add_secret(Secret { name: "ANTHROPIC_API_KEY".into(), ..Default::default() }, "canary-explicit-model-key")
            .unwrap();
        let relay = crate::relay::Service::new(dir.path().join("state"), keychain);
        let legacy = relay.grant_claude("s1").unwrap();
        let grant = relay.grant_secrets("s1", None, None, true, legacy).unwrap().unwrap();
        assert!(grant.placeholder.is_empty());
        assert_eq!(grant.port, 0);
        assert_eq!(relay.live_grants(), 0);
        assert_eq!(grant.secrets["ANTHROPIC_API_KEY"], "canary-explicit-model-key");
        assert_eq!(grant.secrets["ANTHROPIC_BASE_URL"], "https://api.anthropic.com");
        assert!(!format!("{grant:?}").contains("canary"));
    }
}
