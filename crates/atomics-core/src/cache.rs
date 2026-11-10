//! Cache policy shared by configuration, launchers, doctor and clients.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    /// Linked to one store per repository. User settings may opt into unsafe tools.
    pub shared: Vec<String>,
    /// Explicit values win over inherited variables and detected defaults.
    pub env: BTreeMap<String, String>,
    /// Additional untracked output folders to remove. Cargo's target is always detected.
    pub outputs: Vec<String>,
    /// Provenance used to recheck paths in the agent's checkout, which can differ from main.
    #[serde(skip)]
    pub repo_paths: BTreeSet<String>,
}

pub fn relative(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && path.split('/').all(|part| !matches!(part, "" | "." | ".." | ".git"))
        && Path::new(path).components().all(|part| matches!(part, Component::Normal(_)))
}

/// These stores support concurrent access; installed dependencies and build trees do not.
pub fn repo_shared(path: &str) -> bool {
    matches!(path, ".cache/sccache" | ".npm/_cacache" | ".cache/pip" | ".yarn/cache" | ".nuget/packages")
}

pub fn repo_output(path: &str) -> bool {
    relative(path)
        && matches!(path.rsplit('/').next(), Some("target" | "bin" | "obj" | "build" | "dist" | ".next" | "out"))
}

pub fn path_variable(name: &str) -> bool {
    matches!(
        name,
        "SCCACHE_DIR"
            | "npm_config_cache"
            | "NPM_CONFIG_CACHE"
            | "PIP_CACHE_DIR"
            | "YARN_CACHE_FOLDER"
            | "NUGET_PACKAGES"
    )
}

pub fn repo_env(name: &str, value: &str) -> bool {
    if path_variable(name) {
        return relative(value);
    }
    match name {
        "RUSTC_WRAPPER" => matches!(value, "" | "sccache"),
        "CARGO_INCREMENTAL" | "SCCACHE_IGNORE_SERVER_IO_ERROR" => matches!(value, "0" | "1"),
        "CARGO_PROFILE_DEV_DEBUG" | "CARGO_PROFILE_TEST_DEBUG" => {
            matches!(value, "0" | "1" | "2" | "none" | "limited" | "full" | "line-tables-only" | "line-directives-only")
        }
        "SCCACHE_CACHE_SIZE" => {
            let digits = value.trim_end_matches(['K', 'M', 'G', 'T']);
            !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) && digits.len() + 1 >= value.len()
        }
        _ => false,
    }
}

impl CacheConfig {
    pub fn warnings(&self) -> Vec<String> {
        self.shared.iter().filter_map(|path| {
            if !relative(path) {
                Some(format!("cache.shared: {path} must be relative and inside the checkout; skipped"))
            } else if path.split('/').any(|part| matches!(part, "target" | "node_modules" | "bin" | "obj")) {
                Some(format!("cache.shared: {path} is unsafe to share between concurrent worktrees (locks, installed dependencies or mutable build output)"))
            } else {
                None
            }
        }).chain(self.outputs.iter().filter(|path| !relative(path)).map(|path| {
            format!("cache.outputs: {path} must be relative and inside the checkout; skipped")
        })).collect()
    }

    /// Cargo defaults are independent of sccache availability. The caller supplies the machine's
    /// cache directory and resolves sccache in the environment in which the agent will run.
    pub fn agent_env(
        &self,
        cargo: bool,
        sccache: Option<&str>,
        machine_cache: &str,
        inherited: impl Fn(&str) -> Option<String>,
    ) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        if cargo {
            for (key, value) in [
                ("CARGO_INCREMENTAL", "0"),
                ("CARGO_PROFILE_DEV_DEBUG", "line-tables-only"),
                ("CARGO_PROFILE_TEST_DEBUG", "line-tables-only"),
            ] {
                env.insert(key.into(), inherited(key).unwrap_or_else(|| value.into()));
            }
            if let Some(sccache) = sccache {
                for (key, value) in [
                    ("RUSTC_WRAPPER", sccache),
                    ("SCCACHE_DIR", machine_cache),
                    ("SCCACHE_CACHE_SIZE", "10G"),
                    ("SCCACHE_IGNORE_SERVER_IO_ERROR", "1"),
                ] {
                    env.insert(key.into(), inherited(key).unwrap_or_else(|| value.into()));
                }
            }
        }
        env.extend(self.env.clone());
        env
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lean_defaults_work_without_sccache_and_every_value_is_overridable() {
        let mut config = CacheConfig::default();
        let lean = config.agent_env(true, None, "/cache", |_| None);
        assert_eq!(lean["CARGO_INCREMENTAL"], "0");
        assert_eq!(lean["CARGO_PROFILE_TEST_DEBUG"], "line-tables-only");
        assert!(!lean.contains_key("RUSTC_WRAPPER"));
        config.env.insert("CARGO_INCREMENTAL".into(), "1".into());
        config.env.insert("RUSTC_WRAPPER".into(), "".into());
        let env = config.agent_env(true, Some("sccache"), "/cache", |key| {
            (key == "CARGO_PROFILE_DEV_DEBUG").then(|| "full".into())
        });
        assert_eq!(env["CARGO_INCREMENTAL"], "1");
        assert_eq!(env["CARGO_PROFILE_DEV_DEBUG"], "full");
        assert_eq!(env["RUSTC_WRAPPER"], "");
        assert_eq!(env["SCCACHE_DIR"], "/cache");
        assert_eq!(env["SCCACHE_CACHE_SIZE"], "10G");
        assert!(config.agent_env(false, None, "/cache", |_| None).contains_key("RUSTC_WRAPPER"));
    }

    #[test]
    fn repository_allowlist_and_user_warnings_are_distinct() {
        for path in ["target", "node_modules", "src/bin", "obj", "../cache", "/cache", ".git/cache"] {
            assert!(!repo_shared(path));
            assert!(!CacheConfig { shared: vec![path.into()], ..Default::default() }.warnings().is_empty());
        }
        assert!(repo_shared(".npm/_cacache"));
        assert!(repo_output("src/project/obj"));
        assert!(!repo_output("../target"));
        assert!(!repo_env("RUSTC_WRAPPER", "custom-command"));
        assert!(!repo_env("SCCACHE_DIR", "/outside"));
        assert!(repo_env("SCCACHE_DIR", ".cache/sccache"));
        assert!(!repo_env("PATH", "tools"));
    }
}
