//! Host-owned settings, device management and filesystem browsing. No client filesystem APIs.
use super::*;
use crate::protocol::rendezvous::devices::{self, PairingView};
use atomics_core::ops::HostAction;
use atomics_core::projection::{ConfigProposalInfo, DeviceInfo, FolderInfo, ManagementInfo, PairingInfo, RemoteInfo};

impl State {
    pub(super) fn management_info(&self) -> ManagementInfo {
        let mut config = self.management_config.get_or_init(|| self.config_get(None, None).unwrap_or_default()).clone();
        config["theme_tokens"] = serde_json::to_value(&self.theme.tokens).unwrap_or_default();
        let proposals = self
            .proposals
            .iter()
            .take(1)
            .map(|pending| {
                let proposal = &pending.proposal;
                let mut layers = self.config_layers(None);
                // Include proposed secrets as well as the current ones when masking diff context.
                layers.push(atomics_core::selfconfig::Layer::new(
                    atomics_core::selfconfig::Source::Main,
                    &proposal.file,
                    Some(proposal.after.clone()),
                ));
                let cleaner = self.config_cleaner(&layers);
                ConfigProposalInfo {
                    id: pending.id.clone(),
                    file: proposal.file.clone(),
                    scope: proposal.scope.name().into(),
                    from: cleaner.text(&pending.from),
                    reason: pending.reason.as_deref().map(|s| cleaner.text(s)),
                    diff: atomics_core::selfconfig::public_diff(proposal, &cleaner),
                    risks: proposal
                        .risks()
                        .into_iter()
                        .map(|(risk, keys)| format!("⚠ {}: {}", risk.label(), keys.join(", ")))
                        .collect(),
                    warnings: proposal.warnings.iter().map(|s| cleaner.text(s)).collect(),
                    preview: pending
                        .preview
                        .as_ref()
                        .and_then(|theme| serde_json::to_value(&theme.tokens).ok())
                        .map(|value| cleaner.setting(value)),
                }
            })
            .collect();
        let mut remote = RemoteInfo {
            enabled: self.remote.enabled,
            status: self.rendezvous_status().map(|s| format!("{:?}", s.phase)).unwrap_or_else(|| {
                self.remote.addr.map(|addr| format!("Listening on {addr}")).unwrap_or_else(|| "Disabled".into())
            }),
            problems: self.remote.error.iter().cloned().chain(self.journal_problem()).collect(),
            ..Default::default()
        };
        if let Some(devices) = self.remote_devices() {
            remote.problems.extend(devices.problem());
            remote.pairing = Some(match devices.view() {
                PairingView::Idle { note } => PairingInfo::Idle { note: note.or_else(|| devices.unready()) },
                PairingView::Open { link, left, busy } => {
                    PairingInfo::Open { link: Some(link.to_string()), seconds_left: left.as_secs(), busy }
                }
                PairingView::Confirm { name, code, left } => {
                    PairingInfo::Confirm { name, code, seconds_left: left.as_secs() }
                }
            });
            remote.devices = devices
                .list()
                .into_iter()
                .map(|(d, connected)| DeviceInfo {
                    id: devices::digest(&d.key),
                    name: d.name,
                    controls: d.access == atomics_core::noise::Access::Control,
                    connected,
                    paired_ms: d.paired_ms,
                    seen_ms: d.seen_ms,
                })
                .collect();
        }
        let identity_scopes = config["identity_scopes"].clone();
        ManagementInfo {
            config,
            proposals,
            folder_start: paths::home().to_string_lossy().into_owned(),
            folders: self.folder_cache.clone(),
            recent: self.store.recent_projects.iter().take(8).map(|p| p.to_string_lossy().into_owned()).collect(),
            tools: json!({
                "hooks": global_hooks_installed(),
                "sandbox": self.sandbox_lines,
                "sandbox_busy": self.sandbox_check.is_some(),
                "connections": self.connections.management_info(),
                "credentials": self.credential_status,
                "credential_busy": self.credential_busy,
                "credential_kinds": crate::credentials::KINDS.iter().map(|(id, label, _)| json!({"id":id, "label":label})).collect::<Vec<_>>(),
                "identities": self.repos.iter().map(|repo| {
                    let configured = self.project_identity(repo);
                    let identity = self.git_identities.get(repo).map_or(configured.clone(), |git| git.overlay(&configured));
                    let label = self.tickets.workspace_identity_error.clone().unwrap_or_else(|| if identity.name.is_none() && identity.email.is_none() { "Git identity".to_string() } else { identity.label() });
                    let scoped = identity_scopes.as_array().into_iter().flatten()
                        .find(|scope| scope["target"].as_str().is_some_and(|target| Path::new(target) == repo))
                        .map(|scope| scope["identity"].clone()).unwrap_or_else(|| json!({}));
                    json!({"project": repo.to_string_lossy(), "display": paths::display(repo), "label": label, "identity": identity, "override": scoped})
                }).collect::<Vec<_>>(),
                "identity_error": self.tickets.workspace_identity_error,
            }),
            remote,
            themes: self.themes.clone(),
            setup: self.setup_info(),
        }
    }

    pub(super) fn manage(&mut self, action: HostAction) -> Result<serde_json::Value, WireError> {
        let rejected = |s: String| WireError::new(ErrorCode::Rejected, s);
        match action {
            HostAction::SettingsTool { tool } => return self.settings_tool(tool),
            HostAction::BrowseFolder { path } => return self.manage(HostAction::BrowseFolderPage { path, offset: 0 }),
            HostAction::BrowseFolderPage { path, offset } => {
                let folder = self.browse_folder(&path, offset).map_err(rejected)?;
                self.folder_cache.retain(|f| f.page.worktree != folder.page.worktree || f.page.offset != offset);
                if self.folder_cache.len() >= 8 {
                    self.folder_cache.remove(0);
                }
                self.folder_cache.push(folder);
            }
            HostAction::OpenProject { path } => {
                let root = workspace::project_root(&paths::expand(&path)).map_err(|e| rejected(format!("{e:#}")))?;
                self.add_project(&root);
            }
            HostAction::CloseProject { path } => {
                let root = PathBuf::from(path);
                if !self.open_projects.contains(&root) {
                    return Err(WireError::new(ErrorCode::NotFound, "project is not open"));
                }
                self.close_project(&root);
            }
            HostAction::SaveWorkspace { name } => {
                // Validate before the existing save flow, which reports failures through the notice line.
                Workspace::file(&name).map_err(|e| rejected(format!("{e:#}")))?;
                self.save_workspace_checked(&name).map_err(rejected)?;
            }
            HostAction::ConfigPropose { scope, toml, remove, theme, reason } => {
                return self
                    .propose_config(
                        super::selfconfig::ProposedChange {
                            scope,
                            toml: &toml,
                            remove: &remove,
                            theme: theme.as_deref(),
                        },
                        reason,
                        None,
                    )
                    .map_err(rejected);
            }
            HostAction::SelectTheme { name, host } => self.select_theme(&name, host).map_err(rejected)?,
            HostAction::ConfigDecide { id, apply, note } => {
                if !self.proposals.iter().any(|p| p.id == id) {
                    return Err(WireError::new(ErrorCode::NotFound, "proposal is no longer pending"));
                }
                if apply {
                    self.apply_proposal(&id, note);
                } else {
                    self.reject_proposal(&id, note);
                }
                if let Some(config) = self.events.last().and_then(|event| event.config.as_ref())
                    && config["outcome"] == "failed"
                {
                    return Err(rejected(
                        self.config_cleaner(&self.config_layers(None))
                            .text(config["error"].as_str().unwrap_or("Could not apply configuration")),
                    ));
                }
            }
            action => {
                let devices = self
                    .remote_devices()
                    .ok_or_else(|| rejected("Device pairing is not available on this host".into()))?;
                match action {
                    HostAction::PairDevice => {
                        devices.offer(&self.remote.pair_url).map_err(|e| rejected(e.to_string()))?
                    }
                    HostAction::CancelPairing => devices.cancel(),
                    HostAction::DecidePairing { code, controls } => {
                        if !devices.decide_code(
                            &code,
                            controls.map(|control| {
                                if control {
                                    atomics_core::noise::Access::Control
                                } else {
                                    atomics_core::noise::Access::View
                                }
                            }),
                        ) {
                            return Err(WireError::new(
                                ErrorCode::StaleRevision,
                                "pairing changed; compare the current code",
                            ));
                        }
                    }
                    HostAction::RevokeDevice { id } => {
                        let key = devices
                            .list()
                            .into_iter()
                            .find(|(d, _)| devices::digest(&d.key) == id)
                            .map(|(d, _)| d.key)
                            .ok_or_else(|| WireError::new(ErrorCode::NotFound, "device is no longer paired"))?;
                        devices.revoke(&key);
                    }
                    _ => unreachable!("management action"),
                }
            }
        }
        Ok(json!({ "done": true }))
    }

    fn browse_folder(&self, path: &str, offset: u32) -> Result<FolderInfo, String> {
        let folder = paths::expand(path).canonicalize().map_err(|e| format!("Cannot open host folder: {e}"))?;
        let entries = std::fs::read_dir(&folder).map_err(|e| format!("Cannot read host folder: {e}"))?;
        // Unlike a prepared worktree listing, this reads the host filesystem. Bound that read too.
        const MAX_ENTRIES: usize = 16_384;
        let mut directories = Vec::new();
        for (index, entry) in entries.enumerate() {
            if index >= MAX_ENTRIES {
                return Err("Host folder has too many entries to browse".into());
            }
            let entry = entry.map_err(|e| format!("Cannot read host folder: {e}"))?;
            if entry.path().is_dir() {
                directories.push(atomics_core::folder::Entry {
                    path: entry.file_name().to_string_lossy().into_owned(),
                    directory: true,
                });
            }
        }
        let page = atomics_core::folder::entries_page(directories, &folder.to_string_lossy(), "", 0, offset, 8192)
            .map_err(|error| error.message)?;
        Ok(FolderInfo {
            requested: path.into(),
            parent: folder.parent().map(|p| p.to_string_lossy().into_owned()),
            project: workspace::project_root(&folder).is_ok(),
            worktree: git::worktree_of(&folder).map(|w| w.root.to_string_lossy().into_owned()),
            page,
        })
    }
}

impl State {
    fn settings_tool(&mut self, tool: atomics_core::ops::SettingsTool) -> Result<serde_json::Value, WireError> {
        use atomics_core::ops::SettingsTool;
        match tool {
            SettingsTool::CheckSandbox => self.check_sandbox(),
            SettingsTool::RefreshConnections => self.refresh_connections(),
            SettingsTool::ImportConnections => self.import_connections(),
            SettingsTool::Connect { name, store_env } => self.connect_server(&name, store_env, None),
            SettingsTool::Disconnect { name } => self.disconnect_server(&name),
            SettingsTool::AddConnection { name, url } => {
                self.add_connection(&name, &url).map_err(|e| WireError::new(ErrorCode::Rejected, e))?;
            }
            SettingsTool::InstallHooks => {
                let actions = self.action_tx.clone();
                std::thread::spawn(move || {
                    let message = match install_global_hooks() {
                        Ok(_) => "Hooks installed".to_string(),
                        Err(error) => format!("Installing hooks failed: {error:#}"),
                    };
                    let _ = actions.send(Box::new(move |state| state.toast(message)));
                });
            }
            SettingsTool::CleanBuildCaches => self.clean_build_caches(),
            SettingsTool::CheckPrograms => self.check_programs(),
            SettingsTool::SetIsolation { mode } => {
                self.set_isolation(mode).map_err(|e| WireError::new(ErrorCode::Rejected, e))?;
                self.check_sandbox();
            }
            SettingsTool::RunSetup { action, value } => {
                self.run_setup(action, value).map_err(|e| WireError::new(ErrorCode::Conflict, e))?;
            }
            SettingsTool::CancelSetup => self.cancel_setup(),
            SettingsTool::ConnectLater { id, later } => {
                self.connect_later(id, later).map_err(|e| WireError::new(ErrorCode::Rejected, e))?;
            }
            SettingsTool::FinishSetup => {
                self.finish_setup().map_err(|e| WireError::new(ErrorCode::Rejected, e))?;
            }
            SettingsTool::InitProject { path } => {
                self.init_project(&path).map_err(|e| WireError::new(ErrorCode::Rejected, e))?;
            }
            SettingsTool::AnswerStart { run_here } => {
                self.answer_start(run_here).map_err(|e| WireError::new(ErrorCode::Rejected, e))?;
            }
            SettingsTool::SetAzure { enabled } => {
                // The reload takes back what the relay granted when the setting goes off.
                write_azure_enabled(&self.config_path, enabled).map_err(|e| WireError::new(ErrorCode::Rejected, e))?;
                self.reload_config();
            }
            tool => {
                if self.credential_busy {
                    return Err(WireError::new(ErrorCode::Conflict, "A credential operation is already running"));
                }
                let kind = match &tool {
                    SettingsTool::PutCredential { kind, .. } | SettingsTool::RemoveCredential { kind, .. } => {
                        Some(kind)
                    }
                    _ => None,
                };
                if kind.is_some_and(|id| !crate::credentials::KINDS.iter().any(|(known, _, _)| known == id)) {
                    return Err(WireError::new(ErrorCode::Malformed, "Unknown credential kind"));
                }
                self.credential_busy = true;
                let (relay, actions) = (self.relay.clone(), self.action_tx.clone());
                std::thread::spawn(move || {
                    let result = credential_tool(&relay, tool);
                    let _ = actions.send(Box::new(move |state| {
                        state.credential_busy = false;
                        match result {
                            Ok(status) => state.credential_status = status,
                            // What was read last stays beside why this failed.
                            Err(error) => {
                                if !state.credential_status.is_object() {
                                    state.credential_status = json!({});
                                }
                                state.credential_status["error"] = json!(error);
                            }
                        }
                    }));
                });
            }
        }
        Ok(json!({ "accepted": true }))
    }
}

/// Sets `[isolation.sandbox.azure] enabled` in `file` as an applied config change is written: comments kept, a
/// backup, an atomic replace.
pub(super) fn write_azure_enabled(file: &Path, enabled: bool) -> Result<(), String> {
    let before = match std::fs::read_to_string(file) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err("Cannot read user config; fix it before changing Azure access".into()),
    };
    let after = atomics_core::selfconfig::edit(
        before.as_deref().unwrap_or(""),
        &format!("[isolation.sandbox.azure]\nenabled = {enabled}"),
        &[],
    )
    .map_err(|_| "Cannot edit invalid user config; fix it in Edit config first".to_string())?;
    atomics_core::selfconfig::write(file, before.as_deref(), &after, atomics_core::store::now())
        .map_err(|_| "Could not save Azure setting; reload config and retry".to_string())?;
    Ok(())
}

fn credential_tool(
    relay: &crate::relay::Service,
    tool: atomics_core::ops::SettingsTool,
) -> Result<serde_json::Value, String> {
    use atomics_core::ops::SettingsTool;
    let store = crate::credentials::Store::new(relay.keychain().clone());
    match tool {
        SettingsTool::RefreshCredentials => {}
        SettingsTool::PutCredential { kind, value, host } => {
            let label = if kind == "registry" {
                format!("Private registry ({})", host.as_deref().unwrap_or_default())
            } else {
                crate::credentials::KINDS.iter().find(|(id, _, _)| *id == kind).unwrap().1.into()
            };
            relay.edit(&label, |store| store.put(&kind, &value, host.as_deref())).map_err(|error| error.to_string())?;
        }
        SettingsTool::RemoveCredential { kind, host } => {
            let key = match kind.as_str() {
                "registry" => format!("registry:{}", host.as_deref().unwrap_or_default()),
                crate::credentials::GITHUB_APP => {
                    crate::credentials::github_app_id(host.as_deref().unwrap_or_default())
                }
                _ => kind.clone(),
            };
            let label = if kind == "registry" {
                format!("Private registry ({})", host.as_deref().unwrap_or_default())
            } else {
                crate::credentials::KINDS.iter().find(|(id, _, _)| *id == kind).unwrap().1.into()
            };
            relay.edit(&label, |store| store.remove(&key)).map_err(|error| error.to_string())?;
        }
        SettingsTool::CleanCredentials => store.remove_invalid().map_err(|error| error.to_string())?,
        _ => unreachable!("credential tool"),
    }
    let snapshot = store.snapshot().map_err(|error| error.to_string())?;
    let entries: Vec<_> = snapshot
        .entries
        .iter()
        .map(|entry| {
            let (last, sessions) = relay.usage(&entry.label);
            let (kind, host) = entry.editable().unzip();
            json!({
                "label": entry.label,
                "present": entry.present,
                "route": entry.route,
                "kind": kind,
                "host": host.flatten(),
                "last_used": last.map(|s| crate::relay::utc(std::time::UNIX_EPOCH + Duration::from_secs(s))),
                "sessions": sessions,
            })
        })
        .collect();
    let status = crate::relay::azure::status(if cfg!(test) { None } else { crate::setup::find_program("az") });
    let (_, sessions) = relay.usage("Azure CLI");
    let azure = format!(
        "az found: {}; logged in: {}; sessions: {}",
        status.found,
        status.logged_in,
        if sessions.is_empty() { "none".into() } else { sessions.join(", ") }
    );
    Ok(json!({ "entries": entries, "invalid": snapshot.invalid, "azure": azure }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{repository, session, with_state};

    #[test]
    fn remote_open_project_preserves_the_desktop_view() {
        with_state("app::management::tests::remote_open_project_preserves_the_desktop_view", |mut state, _| {
            let (_desktop_dir, desktop) = repository();
            let desktop = desktop.canonicalize().unwrap();
            let (_other_dir, other) = repository();
            let other = other.canonicalize().unwrap();
            state.open_project(&desktop);
            state.sessions = vec![session("desktop-agent", AgentKind::Generic, AgentStatus::Working, &desktop)];
            state.active = Some("desktop-agent".into());
            let toast = state.toast.clone();

            for diff_open in [false, true] {
                state.diff_open = diff_open;
                state.manage(HostAction::OpenProject { path: other.to_string_lossy().into_owned() }).unwrap();
                assert_eq!(state.active.as_deref(), Some("desktop-agent"));
                assert_eq!(state.diff_root.as_deref(), Some(desktop.as_path()));
                assert_eq!(state.diff_open, diff_open);
                assert_eq!(state.toast, toast, "a remote open must not toast on the desktop");
            }

            assert_eq!(state.open_projects, vec![desktop, other.clone()], "reopening must not duplicate it");
            assert_eq!(Store::load(&paths::store_file()).projects, state.open_projects);
            assert!(state.repos.contains(&other), "the shared configuration must include the new project");
        });
    }

    #[test]
    fn opening_a_project_on_the_desktop_still_navigates() {
        with_state("app::management::tests::opening_a_project_on_the_desktop_still_navigates", |mut state, _| {
            let (_desktop_dir, desktop) = repository();
            let desktop = desktop.canonicalize().unwrap();
            let (_other_dir, other) = repository();
            let other = other.canonicalize().unwrap();
            state.open_project(&desktop);
            state.sessions = vec![session("desktop-agent", AgentKind::Generic, AgentStatus::Working, &desktop)];
            state.active = Some("desktop-agent".into());
            state.diff_open = false;
            let below = other.join("src");
            std::fs::create_dir(&below).unwrap();

            state.open_project(&below);

            assert_eq!(state.open_projects, vec![desktop, other.clone()]);
            assert!(state.active.is_none());
            assert_eq!(state.diff_root.as_deref(), Some(other.as_path()));
            assert!(state.diff_open);
            assert_eq!(state.toast.as_ref().unwrap().0, format!("Opened {}", paths::display(&other)));
        });
    }

    #[test]
    fn the_azure_switch_goes_at_once_with_no_proposal_and_keeps_the_rest_of_the_file() {
        use atomics_core::ops::SettingsTool;
        with_state(
            "app::management::tests::the_azure_switch_goes_at_once_with_no_proposal_and_keeps_the_rest_of_the_file",
            |mut state, _| {
                std::fs::write(&state.config_path, "# keep my config\n[voice]\napi_key = 'PRIVATE-value'\n").unwrap();
                state.reload_config();
                for enabled in [true, false] {
                    state.manage(HostAction::SettingsTool { tool: SettingsTool::SetAzure { enabled } }).unwrap();
                    assert_eq!(state.effective_isolation().sandbox.azure.enabled, enabled);
                    assert!(state.proposals.is_empty(), "the user's own switch is not proposed");
                    let values = state.management_info().config;
                    assert_eq!(values["values"]["isolation.sandbox.azure.enabled"]["value"], enabled);
                }
                let written = std::fs::read_to_string(&state.config_path).unwrap();
                assert!(written.starts_with("# keep my config") && written.contains("PRIVATE-value"), "{written}");
            },
        );
    }

    #[test]
    fn credential_status_names_each_entry_and_never_a_value_and_a_failure_says_why() {
        use atomics_core::ops::SettingsTool;
        let (dir, keychain, _store) = crate::credentials::tests::machine(cfg!(target_os = "macos"));
        let relay = crate::relay::Service::transient(dir.path().join("state"), keychain);
        let registry = |status: &serde_json::Value| {
            status["entries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["label"] == "Private registry (registry.example.com)")
                .cloned()
        };
        let put = SettingsTool::PutCredential {
            kind: "registry".into(),
            value: "PRIVATE-typed-value".into(),
            host: Some("registry.example.com".into()),
        };
        let status = credential_tool(&relay, put).unwrap();
        assert!(!status.to_string().contains("PRIVATE-typed-value"), "{status}");
        let entry = registry(&status).expect("the entry is listed");
        assert_eq!(entry["present"], true);
        assert_eq!(entry["kind"], "registry");
        assert_eq!(entry["host"], "registry.example.com");
        assert_eq!(entry["last_used"], serde_json::Value::Null);
        let claude = status["entries"].as_array().unwrap().iter().find(|e| e["label"] == "Claude subscription");
        assert_eq!(claude.unwrap()["present"], false);
        assert_eq!(claude.unwrap()["kind"], "claude");

        let bad = SettingsTool::PutCredential {
            kind: "registry".into(),
            value: "PRIVATE-second-value".into(),
            host: Some("not a host".into()),
        };
        let refused = credential_tool(&relay, bad).unwrap_err();
        assert!(refused.contains("value hidden") && !refused.contains("PRIVATE"), "says why: {refused}");

        let remove =
            SettingsTool::RemoveCredential { kind: "registry".into(), host: Some("registry.example.com".into()) };
        assert!(registry(&credential_tool(&relay, remove).unwrap()).is_none());
    }

    #[test]
    fn a_failed_credential_read_keeps_what_was_read_last_beside_why() {
        use atomics_core::ops::SettingsTool;
        with_state(
            "app::management::tests::a_failed_credential_read_keeps_what_was_read_last_beside_why",
            |mut state, _| {
                let refused = state.manage(HostAction::SettingsTool {
                    tool: SettingsTool::PutCredential { kind: "nope".into(), value: "x".into(), host: None },
                });
                assert_eq!(refused.unwrap_err().code, ErrorCode::Malformed, "an unknown kind goes nowhere");
                assert!(!state.credential_busy);
            },
        );
    }

    #[test]
    fn host_projects_and_proposals_are_visible_and_only_apply_writes() {
        with_state(
            "app::management::tests::host_projects_and_proposals_are_visible_and_only_apply_writes",
            |mut state, _| {
                let (_dir, repo) = repository();
                let repo = repo.canonicalize().unwrap();
                let path = repo.to_string_lossy().into_owned();
                state.manage(HostAction::BrowseFolder { path: path.clone() }).unwrap();
                let info = state.management_info();
                assert!(info.folders.iter().any(|f| f.project && f.worktree.as_deref() == Some(path.as_str())));
                state.manage(HostAction::OpenProject { path }).unwrap();
                assert!(state.host_info().projects.iter().any(|p| Path::new(&p.root) == repo));
                let before = std::fs::read_to_string(&state.config_path).unwrap_or_default();
                let result = state
                    .manage(HostAction::ConfigPropose {
                        scope: atomics_core::selfconfig::Scope::Main,
                        toml: "[layout]\nmin_tile_width = 407.0\n[voice]\napi_key = 'hidden-test-value'".into(),
                        remove: vec![],
                        theme: None,
                        reason: None,
                    })
                    .unwrap();
                let id = result["proposal"].as_str().unwrap().to_string();
                assert_eq!(std::fs::read_to_string(&state.config_path).unwrap_or_default(), before);
                let shown = serde_json::to_string(&state.management_info()).unwrap();
                assert!(!shown.contains("hidden-test-value"), "the config and diff must mask secrets: {shown}");
                state.manage(HostAction::ConfigDecide { id: id.clone(), apply: true, note: String::new() }).unwrap();
                assert_eq!(state.config.layout.min_tile_width, 407.0);
                assert!(state.management_info().proposals.is_empty());
                assert!(state.manage(HostAction::ConfigDecide { id, apply: true, note: String::new() }).is_err());
                assert!(state.manage(HostAction::BrowseFolder { path: "/missing/host/folder".into() }).is_err());
            },
        );
    }

    #[test]
    fn host_folder_pages_are_bounded_and_reach_every_child() {
        with_state("app::management::tests::host_folder_pages_are_bounded_and_reach_every_child", |mut state, _| {
            let folder = paths::home().join("many-folders");
            std::fs::create_dir_all(&folder).unwrap();
            for number in 0..300 {
                std::fs::create_dir(folder.join(format!("project-{number:04}"))).unwrap();
            }
            let path = folder.to_string_lossy().into_owned();
            let mut offset = 0;
            let mut seen = std::collections::BTreeSet::new();
            loop {
                state.manage(HostAction::BrowseFolderPage { path: path.clone(), offset }).unwrap();
                let page = state.folder_cache.last().unwrap();
                assert!(atomics_core::wire::json_len(&page.page) <= 8192);
                seen.extend(page.page.entries.iter().map(|e| e.path.clone()));
                let Some(next) = page.page.next_offset else { break };
                assert!(next > offset);
                offset = next;
            }
            assert_eq!(seen.len(), 300);
        });
    }

    #[test]
    fn host_credential_tools_never_return_stored_or_new_values() {
        use atomics_core::ops::SettingsTool;
        for mac in [true, false] {
            let (dir, keychain, store) = crate::credentials::tests::machine(mac);
            let relay = crate::relay::Service::transient(dir.path().join("state"), keychain);
            let value = "sk-ant-oat01-PRIVATE-NEW-VALUE";
            let status = credential_tool(
                &relay,
                SettingsTool::PutCredential { kind: "claude".into(), value: value.into(), host: None },
            )
            .unwrap();
            assert_eq!(store.token("claude").unwrap().as_deref(), Some(value));
            assert!(!status.to_string().contains(value));
            let entries = status["entries"].as_array().unwrap();
            assert!(entries.iter().any(|entry| entry["label"] == "Claude subscription" && entry["present"] == true));
            credential_tool(&relay, SettingsTool::RemoveCredential { kind: "claude".into(), host: None }).unwrap();
            assert!(store.token("claude").unwrap().is_none());
        }
    }

    #[test]
    fn host_pairing_refuses_stale_codes_and_unavailable_devices() {
        with_state(
            "app::management::tests::host_pairing_refuses_stale_codes_and_unavailable_devices",
            |mut state, _| {
                assert!(state.manage(HostAction::PairDevice).is_err());
                let devices = devices::Devices::open(paths::atomics_home().join("devices.json"), || {});
                state.remote.devices = Some(devices);
                assert_eq!(
                    state
                        .manage(HostAction::DecidePairing { code: "123456".into(), controls: Some(true) })
                        .unwrap_err()
                        .code,
                    ErrorCode::StaleRevision
                );
                assert_eq!(
                    state.manage(HostAction::RevokeDevice { id: "missing".into() }).unwrap_err().code,
                    ErrorCode::NotFound
                );
            },
        );
    }
}
