//! The chief's configuration tools: the effective configuration with each value's source.
//! Everything returned to an agent is cleaned of secrets.
use super::*;
use imacs_core::selfconfig::{self, Layer, Source};
use imacs_core::tickets::Cleaner;

/// A configuration file read for a tool: its text, or the reason it could not be read.
fn read_layer(source: Source, file: &Path) -> Layer {
    match std::fs::read_to_string(file) {
        Ok(text) => Layer::new(source, paths::display(file), Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Layer::new(source, paths::display(file), None),
        Err(err) => Layer::unreadable(source, paths::display(file), err.to_string()),
    }
}

/// A repository's own settings file. Repositories carry no IMACS settings yet; this is
/// where a repository's layer joins, after the workspace.
fn repo_layer(_root: &Path) -> Option<Layer> {
    None
}

impl State {
    /// The files that make up the configuration, lowest precedence first: the main file,
    /// this host's, the active workspace's, and the project's own when given.
    pub(super) fn config_layers(&self, project: Option<&Path>) -> Vec<Layer> {
        let [main, host] = Config::source_paths(&self.config_path);
        let mut layers = vec![read_layer(Source::Main, &main), read_layer(Source::Host, &host)];
        if let Some(file) = self.workspace.as_deref().and_then(|name| Workspace::file(name).ok()) {
            layers.push(read_layer(Source::Workspace, &file));
        }
        layers.extend(project.and_then(repo_layer));
        layers
    }

    /// The configuration IMACS runs with, the workspace's tickets included.
    pub(super) fn effective_config(&self) -> Config {
        Config { tickets: self.effective_tickets(), ..self.config.clone() }
    }

    /// The cleaner for everything the configuration tools return: built from the secrets
    /// the configuration runs with, read from the environment IMACS already knows.
    pub(super) fn config_cleaner(&self, layers: &[Layer]) -> Cleaner {
        Cleaner::new(selfconfig::secrets(&self.effective_config(), layers, crate::setup::known_env_var))
    }

    /// `imacs_config_get`: every effective value with its source, the files involved, and
    /// one project's settings when given. Secret values never leave this function.
    pub(super) fn config_get(&self, project: Option<&str>, key: Option<&str>) -> Result<serde_json::Value, String> {
        let project = match project.map(str::trim).filter(|p| !p.is_empty()) {
            Some(path) => Some(workspace::project_root(&paths::expand(path)).map_err(|err| format!("{err:#}"))?),
            None => None,
        };
        let layers = self.config_layers(project.as_deref());
        let cleaner = self.config_cleaner(&layers);
        let effective = self.effective_config();
        let files: Vec<_> = layers
            .iter()
            .map(|layer| {
                json!({
                    "source": layer.source,
                    "path": layer.path,
                    "exists": layer.text.is_some(),
                    "error": layer.error.as_deref().map(|error| cleaner.error(error)),
                })
            })
            .collect();
        let workspace = self.workspace.as_ref().map(|name| {
            json!({
                "name": name,
                "projects": self.open_projects.iter().map(|p| paths::display(p)).collect::<Vec<_>>(),
            })
        });
        let project = project.map(|root| {
            json!({
                "path": paths::display(&root),
                "open": self.open_projects.contains(&root) || self.repos.contains(&root),
                "sources": layers.iter().map(|layer| layer.source).collect::<Vec<_>>(),
                "new_worktree_example": paths::display(&git::worktree_path(&effective.worktree_path, &root, "example")),
            })
        });
        Ok(json!({
            "host": paths::host_name(),
            "merge": "Built-in defaults, then config.toml, then config.d/<host>.toml: tables merge key by key, arrays and other values replace. A workspace's [tickets] replaces the section as a whole.",
            "files": files,
            "workspace": workspace,
            "project": project,
            "load_error": self.config_error.as_deref().map(|error| cleaner.error(error)),
            "values": selfconfig::values(&effective, &layers, &cleaner, key),
            "secrets": "Values of secret-looking names, credential flags and URL credentials read as ***. Ticket and voice keys stay in the environment variables token_env and api_key_env name.",
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{request, with_app};
    use super::*;

    #[test]
    fn config_get_shows_sources_and_never_secret_values() {
        with_app("app::selfconfig::tests::config_get_shows_sources_and_never_secret_values", |app, _| {
            let [main, host] = Config::source_paths(&app.runtime.config_path);
            std::fs::write(
                &main,
                "base_ref = 'main'\n[voice]\napi_key = 'sk-main-voice-secret'\n[tickets]\nsystem = 'relay'\n\
                 server = { command = ['relay-mcp', '--token', 'cli-token-secret'], env = { RELAY_TOKEN = 'env-token-secret', PROFILE = 'work' } }\n",
            )
            .unwrap();
            std::fs::create_dir_all(host.parent().unwrap()).unwrap();
            std::fs::write(&host, "[layout]\nsidebar_width = 333.0\n").unwrap();
            app.runtime.config = Config::load(&main).unwrap();
            app.runtime.workspace = Some("team".into());
            let file = Workspace::file("team").unwrap();
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(
                &file,
                "projects = []\n[tickets]\nsystem = 'linear'\nserver = { url = 'https://linear.example/mcp', token_env = 'LINEAR_TOKEN' }\n",
            )
            .unwrap();
            app.runtime.apply_ticket_config();

            let response = request(app, json!({"cmd":"config_get"}));
            assert!(response.ok, "{:?}", response.error);
            let result = response.result;
            let text = result.to_string();
            for secret in ["sk-main-voice-secret", "cli-token-secret", "env-token-secret"] {
                assert!(!text.contains(secret), "{secret} leaked: {text}");
            }
            let values = &result["values"];
            assert_eq!(values["base_ref"], json!({"value": "main", "source": "main"}));
            assert_eq!(values["layout.sidebar_width"], json!({"value": 333.0, "source": "host"}));
            assert_eq!(values["layout.diff_min_width"]["source"], "default");
            assert_eq!(values["voice.api_key"], json!({"value": "***", "source": "main"}));
            // The workspace's section replaces the main config's as a whole.
            assert_eq!(values["tickets.system"], json!({"value": "linear", "source": "workspace"}));
            assert_eq!(values["tickets.server.token_env"]["value"], "LINEAR_TOKEN");
            assert!(values.get("tickets.server.env.PROFILE").is_none());
            let sources: Vec<_> = result["files"].as_array().unwrap().iter().map(|f| f["source"].clone()).collect();
            assert_eq!(sources, [json!("main"), json!("host"), json!("workspace")]);
            assert_eq!(result["files"][1]["exists"], true);

            // Without the workspace, the main config's tickets apply, still masked.
            std::fs::write(&file, "projects = []\n").unwrap();
            app.runtime.apply_ticket_config();
            let response = request(app, json!({"cmd":"config_get", "key":"tickets.server"}));
            let values = &response.result["values"];
            assert_eq!(values["tickets.server.env.PROFILE"], json!({"value": "work", "source": "main"}));
            assert_eq!(values["tickets.server.env.RELAY_TOKEN"]["value"], "***");
            assert_eq!(values["tickets.server.command"]["value"], json!(["relay-mcp", "--token", "***"]));
            assert!(values.as_object().unwrap().keys().all(|key| key.starts_with("tickets.server.")));
            for secret in ["cli-token-secret", "env-token-secret"] {
                assert!(!response.result.to_string().contains(secret), "{}", response.result);
            }

            // A broken host file is reported by place, without quoting its line.
            std::fs::write(&host, "[layout]\nsidebar_width = sk-unquoted-secret\n").unwrap();
            let response = request(app, json!({"cmd":"config_get"}));
            let error = response.result["files"][1]["error"].as_str().unwrap();
            assert!(error.starts_with("line 2"), "{error}");
            assert!(!response.result.to_string().contains("sk-unquoted-secret"));

            let (_dir, repo) = super::super::tests::repository();
            let response = request(app, json!({"cmd":"config_get", "project": repo, "key":"base_ref"}));
            assert!(response.ok, "{:?}", response.error);
            assert_eq!(response.result["project"]["path"], paths::display(&repo.canonicalize().unwrap()));
            assert_eq!(response.result["project"]["sources"], json!(["main", "host", "workspace"]));
            assert!(!request(app, json!({"cmd":"config_get", "project": "/nonexistent/imacs"})).ok);
        });
    }
}
