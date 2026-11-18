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
        Self::with_sync(info, crate::vm::built::running_bundle, crate::relay::Keychain::current(), secrets::sync, wake)
    }

    fn with_sync(
        mut info: Option<BundleInfo>,
        running_bundle: impl Fn(&BundleInfo) -> anyhow::Result<BundleInfo> + Send + 'static,
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
                        Command::ConnectGithub => {
                            sync = true;
                            let connected = connect_github(&relay, |device| {
                                let mut status = refresh().unwrap_or_else(|_| json!({}));
                                status["github_device"] = device;
                                let _ = updates.send((status, false));
                                wake();
                            });
                            connected.and_then(|()| refresh())
                        }
                        Command::Manage(HostAction::SettingsTool { tool }) => {
                            sync = matches!(
                                tool,
                                SettingsTool::AddSecret { .. }
                                    | SettingsTool::RemoveSecret { .. }
                                    | SettingsTool::SetGithubPolicy { .. }
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
                        let result = match &mut info {
                            Some(info) => running_bundle(info).and_then(|running| {
                                let result =
                                    synchronize(&running, &crate::credentials::Store::local(relay.keychain().clone()));
                                *info = running;
                                result
                            }),
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

    pub fn host_connected(&mut self) {
        self.refresh();
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

/// Device authorization runs where the keychain is: never on the VM host protocol.
fn connect_github(relay: &crate::relay::Service, show: impl Fn(Value)) -> Result<(), String> {
    connect_github_at(relay, &crate::github::Endpoints::default(), &crate::connections::Browser::current(), show)
}

fn connect_github_at(
    relay: &crate::relay::Service,
    endpoints: &crate::github::Endpoints,
    browser: &crate::connections::Browser,
    show: impl Fn(Value),
) -> Result<(), String> {
    use std::time::{Duration, Instant};
    let api = endpoints.api("github.com", None);
    let code = api.device_code(atomics_core::github::DEFAULT_CLIENT_ID).map_err(|error| {
        let message = format!("GitHub device-code request failed: {error}");
        eprintln!("atomics: {message}");
        message
    })?;
    let mut device = json!({"code":code.user_code,"url":code.verification_uri});
    show(device.clone());
    if let Err(error) = browser.open(&code.verification_uri) {
        device["open_error"] = json!(format!("Could not open the browser: {error}. Open the link to authorize."));
        show(device);
    }
    let deadline = Instant::now() + Duration::from_secs(code.expires_in);
    let mut interval = code.interval;
    loop {
        std::thread::sleep(Duration::from_secs(interval));
        if Instant::now() >= deadline {
            return Err("GitHub code expired; connect again".into());
        }
        match api.device_token(atomics_core::github::DEFAULT_CLIENT_ID, &code.device_code).map_err(|e| e.to_string())? {
            crate::github::DevicePoll::Pending => {}
            crate::github::DevicePoll::SlowDown => interval += 5,
            crate::github::DevicePoll::Failed(error) => return Err(error),
            crate::github::DevicePoll::Token(grant) => {
                let login =
                    endpoints.api("github.com", Some(grant.token.clone())).viewer().map_err(|e| e.to_string())?;
                crate::credentials::Store::local(relay.keychain().clone())
                    .connect_github_account(&login, grant.token.secret())
                    .map_err(|e| e.to_string())?;
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomics_core::secrets::Secret;
    use atomics_ui::Backend;
    use std::process::Command as Process;
    use std::time::{Duration, Instant};

    #[test]
    fn github_device_link_survives_failed_browser_open_and_authorizes_locally() {
        let (dir, keychain, store) = crate::credentials::tests::machine(false);
        let relay = crate::relay::Service::transient(dir.path().to_path_buf(), keychain);
        let fake = crate::github::fake::FakeGithub::start("gho_PRIVATE-device");
        fake.authorize_device();
        let endpoints = crate::github::Endpoints { base: Some(fake.url.clone()), ..Default::default() };
        let shown = std::sync::Mutex::new(Vec::new());
        connect_github_at(&relay, &endpoints, &crate::connections::Browser::at("/usr/bin/false"), |status| {
            shown.lock().unwrap().push(status);
        })
        .unwrap();
        let shown = shown.lock().unwrap();
        let status = shown.last().unwrap();
        assert_eq!(status["code"], "WDJB-MJHT");
        assert_eq!(status["url"], "https://github.com/login/device");
        assert!(status["open_error"].as_str().is_some_and(|error| error.contains("browser")), "{status}");
        assert!(!status.to_string().contains("PRIVATE"));
        assert!(
            store
                .secrets()
                .unwrap()
                .iter()
                .any(|secret| secret.name == crate::relay::github::account_spec("alice").unwrap().name)
        );
    }

    #[test]
    fn github_device_request_error_has_context_and_shows_no_code() {
        let (dir, keychain, _) = crate::credentials::tests::machine(false);
        let relay = crate::relay::Service::transient(dir.path().to_path_buf(), keychain);
        let endpoints = crate::github::Endpoints { base: Some("http://127.0.0.1:0".into()), ..Default::default() };
        let error = connect_github_at(&relay, &endpoints, &crate::connections::Browser::at("/usr/bin/false"), |_| {
            panic!("a failed request must not offer a code");
        })
        .unwrap_err();
        assert!(error.starts_with("GitHub device-code request failed:"), "{error}");
    }

    #[test]
    fn vm_window_adds_locally_syncs_over_admin_stdin_reconnects_and_removes() {
        let (dir, keychain, store) = crate::credentials::tests::machine(false);
        const PREVIOUS: &str = "/nix/store/00000000000000000000000000000000-previous";
        const NEXT: &str = "/nix/store/11111111111111111111111111111111-next";
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vm-account.py");
        let info = BundleInfo {
            version: 1,
            instance: "vm".into(),
            user: "atomics".into(),
            home: "/home/atomics".into(),
            mac_user: "user".into(),
            install_dir: dir.path().display().to_string(),
            host: None,
            admin: Some("admin".into()),
            build: None,
        };
        let metadata = dir.path().join("boot-accounts.json");
        let boot = dir.path().join("running");
        std::fs::write(&boot, PREVIOUS).unwrap();
        let table = metadata.clone();
        let record = dir.path().to_path_buf();
        let mut worker = Credentials::with_sync(
            Some(info.clone()),
            move |info| {
                crate::vm::built::BootAccounts::load(&table)?
                    .running(&table, info, |_| Ok(std::fs::read_to_string(&boot)?))
            },
            keychain,
            move |info, store| {
                std::fs::write(record.join("selected"), "")?;
                let broker = secrets::AdminBroker(|bytes: &[u8]| {
                    secrets::send_verified(info, &record.join("boot-accounts.json"), bytes, |candidate| {
                        crate::vm::account::Connection::open_with(Duration::from_secs(5), |local| {
                            let template = crate::vm::account::command(candidate, local, true);
                            let mut command = Process::new("python3");
                            command.arg(&script).args(template.get_args()).env("RECORD", &record);
                            Ok(command)
                        })
                    })
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
        let updated = BundleInfo { admin: Some("next-admin".into()), ..info.clone() };
        crate::vm::built::BootAccounts::default().record(&metadata, (PREVIOUS, &info), (NEXT, &updated)).unwrap();
        worker.refresh();
        settle(&mut worker);
        let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
        assert!(argv.contains("-l\nadmin\n"), "a staged update can leave the old guest running: {argv}");
        worker.host_connected();
        settle(&mut worker);
        let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
        assert!(argv.contains("-l\nadmin\n"), "a reconnect before reboot still uses the running administrator");
        let lock_path = dir.path().join("journal/vm-secrets.lock");
        let lock = std::fs::File::open(lock_path).unwrap();
        lock.lock().unwrap();
        std::fs::remove_file(dir.path().join("selected")).unwrap();
        worker.refresh();
        // The route was selected before the worker entered the journal lock.
        assert!(!worker.updates.recv_timeout(Duration::from_secs(5)).unwrap().1);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !dir.path().join("selected").exists() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        std::fs::remove_file(dir.path().join("stdin")).unwrap();
        std::fs::write(dir.path().join("running"), NEXT).unwrap();
        lock.unlock().unwrap();
        settle(&mut worker);
        assert_eq!(worker.status["secrets"][0]["vm_sync"], "Synced");
        let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
        assert!(argv.contains("-l\nnext-admin\n"), "the waiting mutation must select the post-reboot account");
        worker.host_connected();
        settle(&mut worker);
        assert_eq!(worker.status["secrets"][0]["vm_sync"], "Synced");
        let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
        assert!(argv.contains("-l\nnext-admin\n"), "the open window uses the updated bundle's admin: {argv}");
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
