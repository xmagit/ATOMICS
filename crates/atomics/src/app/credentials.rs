//! Credential I/O belongs to a worker, never the UI/state lock. The client only keeps a masked
//! edit buffer and value-free status; submitting moves the buffer straight to that worker.
use super::*;
use crate::credentials::{KINDS, Snapshot, Store};

struct StatusView {
    snapshot: Option<Snapshot>,
    lines: Vec<String>,
    azure: String,
}
type Checked = Result<StatusView, String>;

fn write_azure_enabled(file: &Path, enabled: bool) -> Result<(), String> {
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

#[derive(Default)]
pub(super) struct CredentialUi {
    selected: usize,
    value: String,
    host: String,
    snapshot: Option<Snapshot>,
    lines: Vec<String>,
    azure: String,
    pending: Option<Receiver<Checked>>,
}

enum Action {
    Refresh,
    Put(String, Option<String>),
    Remove(Option<String>),
    Clean,
    Azure { enabled: bool, file: PathBuf },
}

impl CredentialUi {
    fn start(&mut self, relay: Arc<crate::relay::Service>, action: Action, ctx: egui::Context) {
        if self.pending.is_some() {
            return;
        }
        let (id, selected_label, _) = KINDS[self.selected];
        let (tx, rx) = std::sync::mpsc::channel();
        self.pending = Some(rx);
        std::thread::spawn(move || {
            let store = Store::new(relay.keychain().clone());
            let result = (|| {
                match action {
                    Action::Refresh => {}
                    Action::Put(value, host) => {
                        let label = if id == "registry" {
                            format!("Private registry ({})", host.as_deref().unwrap_or_default())
                        } else {
                            selected_label.into()
                        };
                        relay
                            .edit(&label, |store| store.put(id, &value, host.as_deref()))
                            .map_err(|error| error.to_string())?;
                    }
                    Action::Remove(host) => {
                        let key = if id == "registry" {
                            format!("registry:{}", host.as_deref().unwrap_or_default())
                        } else {
                            id.into()
                        };
                        let label = if id == "registry" {
                            format!("Private registry ({})", host.as_deref().unwrap_or_default())
                        } else {
                            selected_label.into()
                        };
                        relay.edit(&label, |store| store.remove(&key)).map_err(|error| error.to_string())?;
                    }
                    Action::Clean => store.remove_invalid().map_err(|error| error.to_string())?,
                    Action::Azure { enabled, file } => {
                        write_azure_enabled(&file, enabled)?;
                        if !enabled {
                            relay.revoke_azure();
                        }
                    }
                }
                let status =
                    crate::relay::azure::status(if cfg!(test) { None } else { crate::setup::find_program("az") });
                let (_, sessions) = relay.usage("Azure CLI");
                let azure = format!(
                    "az found: {}; logged in: {}; sessions: {}",
                    status.found,
                    status.logged_in,
                    if sessions.is_empty() { "none".into() } else { sessions.join(", ") }
                );
                let snapshot = store.snapshot();
                let mut lines = match &snapshot {
                    Ok(snapshot) => snapshot.lines(),
                    Err(error) => vec![error.to_string()],
                };
                if let Ok(snapshot) = &snapshot {
                    for entry in &snapshot.entries {
                        let (last, sessions) = relay.usage(&entry.label);
                        let last = last
                            .map(|seconds| crate::relay::utc(std::time::UNIX_EPOCH + Duration::from_secs(seconds)))
                            .unwrap_or_else(|| "never".into());
                        lines.push(format!(
                            "{} — last used: {last}; sessions: {}",
                            entry.label,
                            if sessions.is_empty() { "none".into() } else { sessions.join(", ") }
                        ));
                    }
                }
                Ok(StatusView { snapshot: snapshot.ok(), lines, azure })
            })();
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    fn poll(&mut self) {
        let Some(rx) = &self.pending else { return };
        match rx.try_recv() {
            Ok(Ok(status)) => {
                self.snapshot = status.snapshot;
                self.lines = status.lines;
                self.azure = status.azure;
            }
            Ok(Err(error)) => self.lines = vec![error],
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.lines = vec!["Credential worker stopped; refresh to retry".into()]
            }
        }
        self.pending = None;
    }

    fn draw(&mut self, ui: &mut egui::Ui, mut azure_enabled: bool, file: &Path) -> Option<Action> {
        self.poll();
        ui.set_max_width(620.0);
        let mut action = None;
        if self.snapshot.is_none() && self.pending.is_none() && self.lines.is_empty() {
            action = Some(Action::Refresh);
        }
        ui.label(RichText::new("Azure CLI").strong());
        if ui
            .add_enabled(self.pending.is_none(), egui::Checkbox::new(&mut azure_enabled, "Enable Azure CLI relay"))
            .changed()
        {
            action = Some(Action::Azure { enabled: azure_enabled, file: file.to_path_buf() });
        }
        ui.label(if azure_enabled {
            "Host Azure login through the relay"
        } else {
            "Off — no Azure placeholders are granted"
        });
        ui.label(&self.azure);
        ui.separator();
        egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
            for line in &self.lines {
                ui.add(egui::Label::new(line).wrap());
            }
        });
        if self.pending.is_some() {
            ui.label("Waiting for OS credential store…");
        }
        ui.add_enabled_ui(self.pending.is_none(), |ui| {
            if ui.button("Refresh credentials").clicked() { action = Some(Action::Refresh); }
            ui.separator();
            egui::ComboBox::from_id_salt("credential-type").selected_text(KINDS[self.selected].1).show_ui(ui, |ui| {
                for (index, kind) in KINDS.iter().enumerate() {
                    if ui.selectable_value(&mut self.selected, index, kind.1).changed() {
                        self.value.clear();
                        self.host.clear();
                    }
                }
            });
            if KINDS[self.selected].0 == "registry" {
                ui.add(egui::TextEdit::singleline(&mut self.host).hint_text("registry.example.com (host only)"));
            }
            ui.add(egui::TextEdit::singleline(&mut self.value).password(true).hint_text("New value (hidden)"));
            ui.label("Stored values are never displayed. Replace/remove revokes existing session placeholders; restart those sessions.");
            ui.horizontal(|ui| {
                if ui.add_enabled(!self.value.is_empty(), egui::Button::new("Add / replace credential")).clicked() {
                    let host = (KINDS[self.selected].0 == "registry").then(|| self.host.clone());
                    action = Some(Action::Put(std::mem::take(&mut self.value), host));
                }
                if ui.button("Remove credential").clicked() {
                    self.value.clear();
                    action = Some(Action::Remove((KINDS[self.selected].0 == "registry").then(|| self.host.clone())));
                }
            });
            if self.snapshot.as_ref().is_some_and(|snapshot| snapshot.invalid > 0)
                && ui.button("Remove unknown / misfiled entries").clicked() { action = Some(Action::Clean); }
        });
        ui.separator();
        ui.label(
            "Bare Claude can opt in with [isolation] bare_relay = true in config. The chief follows this setting too.",
        );
        action
    }
}

impl Desktop<'_> {
    pub(super) fn credentials_menu(&mut self, ui: &mut egui::Ui) {
        let mut open = false;
        ui.menu_button("Credentials", |ui| {
            open = true;
            if let Some(action) = self.client.credentials.draw(
                ui,
                self.runtime.effective_isolation().sandbox.azure.enabled,
                &self.runtime.config_path,
            ) {
                self.client.credentials.start(self.runtime.relay.clone(), action, ui.ctx().clone());
            }
        });
        if !open {
            self.client.credentials.value.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn azure_settings_switch_round_trips_user_config() {
        fn render(
            form: &mut CredentialUi,
            ctx: &egui::Context,
            file: &Path,
            events: Vec<egui::Event>,
        ) -> (egui::FullOutput, Option<Action>) {
            let enabled = Config::load(file).unwrap().isolation.sandbox.azure.enabled;
            let mut action = None;
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 1200.0))),
                    time: Some(ctx.input(|input| input.time) + 0.1),
                    events,
                    ..Default::default()
                },
                |ui| {
                    action = form.draw(ui, enabled, file);
                },
            );
            (output, action)
        }
        let (dir, keychain, store) = crate::credentials::tests::machine(cfg!(target_os = "macos"));
        let file = dir.path().join("config.toml");
        std::fs::write(&file, "# keep my config\n[voice]\napi_key = 'PRIVATE-existing-value'\n").unwrap();
        let relay = Arc::new(crate::relay::Service::transient(dir.path().join("state"), keychain));
        let ctx = egui::Context::default();
        let mut form = CredentialUi { snapshot: Some(store.snapshot().unwrap()), ..Default::default() };
        assert!(!Config::load(&file).unwrap().isolation.sandbox.azure.enabled);
        for expected in [true, false] {
            render(&mut form, &ctx, &file, vec![]).0.drop_without_applying_deltas();
            let (output, _) = render(&mut form, &ctx, &file, vec![]);
            let pos = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == "Enable Azure CLI relay" => {
                        Some(text.pos + text.galley.rect.center().to_vec2())
                    }
                    _ => None,
                })
                .expect("Azure enable switch is visible");
            assert!(!format!("{:?}", output.shapes).contains("PRIVATE-existing-value"));
            output.drop_without_applying_deltas();
            let mut selected = None;
            for pressed in [true, false] {
                let (output, action) = render(
                    &mut form,
                    &ctx,
                    &file,
                    vec![
                        egui::Event::PointerMoved(pos),
                        egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: Default::default(),
                        },
                    ],
                );
                output.drop_without_applying_deltas();
                if action.is_some() {
                    selected = action;
                }
            }
            assert!(matches!(&selected, Some(Action::Azure { enabled, .. }) if *enabled == expected));
            form.start(relay.clone(), selected.unwrap(), ctx.clone());
            let deadline = Instant::now() + Duration::from_secs(20);
            while form.pending.is_some() {
                form.poll();
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(Config::load(&file).unwrap().isolation.sandbox.azure.enabled, expected);
            let text = std::fs::read_to_string(&file).unwrap();
            assert!(text.contains("# keep my config") && text.contains("PRIVATE-existing-value"));
            assert!(!format!("{:?} {}", form.lines, form.azure).contains("PRIVATE-existing-value"));
        }
    }

    #[test]
    fn credentials_settings_draws_status_and_masks_new_values() {
        for mac in [true, false] {
            let (_dir, keychain, store) = crate::credentials::tests::machine(mac);
            let token = "sk-ant-oat01-PRIVATE-STORED";
            store.put("claude", token, None).unwrap();
            let relay = Arc::new(crate::relay::Service::transient(_dir.path().join("state"), keychain));
            let ctx = egui::Context::default();
            let mut form = CredentialUi { value: "PRIVATE-NEW-VALUE".into(), ..Default::default() };
            form.start(relay.clone(), Action::Refresh, ctx.clone());
            let deadline = Instant::now() + Duration::from_secs(20);
            while form.pending.is_some() {
                form.poll();
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(900.0, 1000.0))),
                    ..Default::default()
                },
                |ui| {
                    form.draw(ui, false, Path::new("/fixture/config.toml"));
                },
            );
            let text = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("Claude subscription: present"), "{text}");
            assert!(text.contains("last used: never; sessions: none"), "{text}");
            assert!(!text.contains(token) && !text.contains("PRIVATE-NEW-VALUE"));
            output.drop_without_applying_deltas();
            form.start(relay.clone(), Action::Put("sk-ant-oat01-REPLACED".into(), None), ctx.clone());
            while form.pending.is_some() {
                form.poll();
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(store.token("claude").unwrap().as_deref(), Some("sk-ant-oat01-REPLACED"));
            form.start(relay, Action::Remove(None), ctx);
            while form.pending.is_some() {
                form.poll();
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(store.token("claude").unwrap().is_none());
        }
    }

    #[test]
    fn credentials_bare_claude_launch_uses_a_placeholder_and_revokes_it_on_exit() {
        super::super::tests::with_app(
            "app::credentials::tests::credentials_bare_claude_launch_uses_a_placeholder_and_revokes_it_on_exit",
            |app, ctx| {
                use std::os::unix::fs::PermissionsExt;
                let (dir, keychain, store) = crate::credentials::tests::machine(cfg!(target_os = "macos"));
                let token = "sk-ant-oat01-PRIVATE-BARE";
                store.put("claude", token, None).unwrap();
                let script = dir.path().join("claude-probe");
                let output = dir.path().join("env-and-args");
                std::fs::write(
                    &script,
                    format!(
                        "#!/bin/sh\nenv > '{}'\nprintf '%s\\n' \"$@\" >> '{}'\n",
                        output.display(),
                        output.display()
                    ),
                )
                .unwrap();
                std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
                let relay = Arc::new(crate::relay::Service::transient(dir.path().join("state"), keychain));
                app.runtime.relay = relay.clone();
                app.runtime.config.isolation.mode = atomics_core::isolation::IsolationMode::Bare;
                app.runtime.config.isolation.bare_relay = true;
                app.runtime.config.agents.push(atomics_core::config::AgentConfig {
                    name: "relay-probe".into(),
                    command: vec![script.display().to_string()],
                    kind: AgentKind::Claude,
                    busy_text: None,
                    isolation: None,
                });
                let session =
                    app.runtime.start(ctx, "relay-probe", dir.path().to_path_buf(), None, vec![], None).unwrap();
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(30));
                loop {
                    app.runtime.runtime_tick(ctx);
                    if output.exists()
                        && relay.live_grants() == 0
                        && app.runtime.sessions.iter().any(|s| s.id == session && s.terminal.is_some())
                    {
                        break;
                    }
                    assert!(Instant::now() < deadline, "bare relay launch did not finish");
                    std::thread::sleep(Duration::from_millis(10));
                }
                let output = std::fs::read_to_string(output).unwrap();
                assert!(output.contains("CLAUDE_CODE_OAUTH_TOKEN=sk-ant-oat01-atomics-"), "{output}");
                assert!(output.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:"), "{output}");
                assert!(!output.contains(token));
                assert!(output.contains("hooks"), "host hooks survive relay settings");
            },
        );
    }
}
