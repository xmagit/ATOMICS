//! The native VM window's credential worker. The remote backend queues these commands locally;
//! only value-free status goes back to the UI. Keychain and SSH I/O stay off the drawing thread.
use std::sync::{Arc, mpsc};

use atomics_core::ops::{HostAction, SettingsTool};
use atomics_ui::Command;
use serde_json::{Value, json};

use crate::vm::{BundleInfo, secrets};

type Update = (Value, bool);

pub struct Credentials {
    commands: mpsc::Sender<Command>,
    updates: mpsc::Receiver<Update>,
    status: Value,
    pending: usize,
}

impl Credentials {
    pub fn new(info: Option<BundleInfo>, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self::with_sync(info, crate::relay::Keychain::current(), secrets::sync, wake)
    }

    fn with_sync(
        info: Option<BundleInfo>,
        keychain: crate::relay::Keychain,
        synchronize: impl Fn(&BundleInfo, &crate::credentials::Store) -> anyhow::Result<secrets::Report> + Send + 'static,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let (commands, requests) = mpsc::channel();
        let (updates, results) = mpsc::channel();
        std::thread::Builder::new()
            .name("atomics-client-credentials".into())
            .spawn(move || {
                let relay = crate::relay::Service::transient(atomics_core::paths::atomics_home(), keychain);
                let mut last_report = None;
                for command in requests {
                    let refresh =
                        || crate::app::management::local_credential_tool(&relay, SettingsTool::RefreshCredentials);
                    let mut copy_note = None;
                    let mut sync;
                    let result = match command {
                        Command::Manage(HostAction::SettingsTool { tool }) => {
                            sync = matches!(
                                tool,
                                SettingsTool::AddSecret { .. }
                                    | SettingsTool::RemoveSecret { .. }
                                    | SettingsTool::RefreshCredentials
                            );
                            crate::app::management::local_credential_tool(&relay, tool)
                        }
                        Command::CopySecret(name) => {
                            sync = false;
                            copy_note = Some(match crate::secret_copy::copy(relay.keychain().clone(), &name) {
                                Ok(()) => "Secret copied to the clipboard".into(),
                                Err(error) => error.to_string(),
                            });
                            refresh()
                        }
                        _ => continue,
                    };
                    let mut status = match result {
                        Ok(status) => status,
                        Err(error) => {
                            let mut status = refresh().unwrap_or_else(|_| json!({}));
                            status["error"] = json!(error);
                            sync = false;
                            status
                        }
                    };
                    status["copy_note"] = json!(copy_note);
                    if sync {
                        decorate(&mut status, &info, None);
                        let _ = updates.send((status.clone(), false));
                        wake();
                        let result = match &info {
                            Some(info) => {
                                synchronize(info, &crate::credentials::Store::local(relay.keychain().clone()))
                            }
                            None => Err(anyhow::anyhow!("VM bundle unavailable; run atomics vm setup")),
                        };
                        last_report = Some(result.map_err(|error| error.to_string()));
                        decorate(&mut status, &info, last_report.as_ref());
                    } else {
                        // A local integration edit or Copy does not imply a fresh broker acknowledgement.
                        decorate(&mut status, &info, last_report.as_ref());
                    }
                    let _ = updates.send((status, true));
                    wake();
                }
            })
            .expect("starting client credential worker");
        Self { commands, updates: results, status: json!({}), pending: 0 }
    }

    pub fn request(&mut self, command: Command) {
        if self.commands.send(command).is_ok() {
            self.pending += 1;
        } else {
            self.status["error"] = json!("Credential worker stopped; reopen this window to retry");
        }
    }

    pub fn refresh(&mut self) {
        self.request(Command::Manage(HostAction::SettingsTool { tool: SettingsTool::RefreshCredentials }));
    }

    pub fn poll(&mut self, remote: &atomics_web::remote::RemoteBackend) {
        for command in remote.take_native_credentials() {
            self.request(command);
        }
        for (status, done) in self.updates.try_iter() {
            self.status = status;
            if done {
                self.pending = self.pending.saturating_sub(1);
            }
        }
        remote.set_native_credentials(self.status.clone(), self.pending > 0);
    }
}

fn decorate(status: &mut Value, info: &Option<BundleInfo>, report: Option<&Result<secrets::Report, String>>) {
    status["vm_warning"] = json!(info.as_ref().filter(|info| info.admin.is_none()).map(|_| secrets::ONE_TRUST_DOMAIN));
    if let Some(rows) = status["secrets"].as_array_mut() {
        for row in rows {
            let name = row["spec"]["name"].as_str().unwrap_or_default();
            row["vm_sync"] = json!(match report {
                Some(Ok(report)) => report.rows.get(name).cloned().unwrap_or_else(|| "Pending".into()),
                Some(Err(error)) => format!("Failed: {error}"),
                None => "Pending".into(),
            });
        }
    }
    status["vm_removals"] = match report {
        Some(Ok(report)) => json!(report.removals),
        Some(Err(error)) => {
            status["error"] = json!(format!("VM sync failed: {error}"));
            json!({})
        }
        None => json!({}),
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomics_core::secrets::Secret;
    use atomics_ui::Backend;
    use std::process::Command as Process;
    use std::time::{Duration, Instant};

    #[test]
    fn vm_window_adds_locally_syncs_over_admin_stdin_reconnects_and_removes() {
        let (dir, keychain, store) = crate::credentials::tests::machine(false);
        let script = dir.path().join("ssh");
        crate::test_support::write_program(
            &script,
            r#"#!/bin/sh
printf '%s\n' "$@" > "$RECORD/argv"
cat > "$RECORD/stdin"
test ! -f "$RECORD/offline"
"#,
        );
        let info = BundleInfo {
            version: 1,
            instance: "vm".into(),
            user: "atomics".into(),
            home: "/home/atomics".into(),
            mac_user: "user".into(),
            install_dir: "/bundle".into(),
            host: None,
            admin: Some("admin".into()),
            build: None,
        };
        let record = dir.path().to_path_buf();
        let mut worker = Credentials::with_sync(
            Some(info),
            keychain,
            move |info, store| {
                let broker = secrets::AdminBroker(|| {
                    let template = secrets::command(info)?;
                    let mut command = Process::new(&script);
                    command.args(template.get_args()).env("RECORD", &record);
                    Ok(command)
                });
                secrets::sync_with(&info.instance, store, &broker, &record.join("journal"))
            },
            Arc::new(|| {}),
        );
        let remote = atomics_web::remote::RemoteBackend::new("vm");
        remote.enable_native_credentials(None);
        let settle = |worker: &mut Credentials| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                worker.poll(&remote);
                if worker.pending == 0 {
                    break;
                }
                assert!(Instant::now() < deadline, "credential worker stuck");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let canary = "canary-native-window";
        std::fs::write(dir.path().join("offline"), "").unwrap();
        remote.send(Command::Manage(HostAction::SettingsTool {
            tool: SettingsTool::AddSecret {
                spec: Secret {
                    name: "KEY".into(),
                    host: Some("api.example.com".into()),
                    scope: atomics_core::secrets::Scope::Project("/vm-only/main-checkout".into()),
                    ..Default::default()
                },
                value: canary.into(),
            },
        }));
        worker.poll(&remote);
        let (pending, done) = worker.updates.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(!done);
        assert_eq!(pending["secrets"][0]["vm_sync"], "Pending");
        assert!(!pending.to_string().contains(canary));
        settle(&mut worker);
        assert_eq!(store.secrets().unwrap()[0].name, "KEY", "local keychain remains the source of truth after failure");
        assert!(worker.status["secrets"][0]["vm_sync"].as_str().unwrap().starts_with("Failed:"));
        assert!(!worker.status.to_string().contains(canary));
        assert!(remote.take_outgoing().is_empty());
        let request: Value = serde_json::from_slice(&std::fs::read(dir.path().join("stdin")).unwrap()).unwrap();
        assert_eq!(request["value"], canary);
        assert_eq!(request["spec"]["scope"]["target"], "/vm-only/main-checkout");
        let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
        assert!(argv.contains("-l\nadmin\n") && argv.contains("ControlPath=none"));
        assert!(!argv.contains(canary));
        std::fs::remove_file(dir.path().join("offline")).unwrap();
        worker.refresh(); // The same entry point used on every successful reconnect.
        settle(&mut worker);
        assert_eq!(worker.status["secrets"][0]["vm_sync"], "Synced");
        remote.send(Command::Manage(HostAction::SettingsTool {
            tool: SettingsTool::RemoveSecret { name: "KEY".into() },
        }));
        settle(&mut worker);
        assert!(store.secrets().unwrap().is_empty());
        let request: Value = serde_json::from_slice(&std::fs::read(dir.path().join("stdin")).unwrap()).unwrap();
        assert_eq!(request, json!({"op":"remove", "name":"KEY"}));
        assert!(worker.status["secrets"].as_array().unwrap().is_empty());
        assert!(remote.take_outgoing().is_empty());
    }
}
