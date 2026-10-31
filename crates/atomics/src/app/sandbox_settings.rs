//! Sandbox readiness is checked on demand, off the UI thread, using doctor's checks.
use super::*;

impl State {
    fn check_sandbox(&mut self) {
        if self.sandbox_check.is_some() {
            return;
        }
        let program = resolve_program(&self.sbx);
        let config = match crate::sandbox::checks::configured(&self.config, self.workspace.as_deref()) {
            Ok(config) => config,
            Err(_) => {
                self.sandbox_lines = vec!["✗ Cannot read sandbox/ticket configuration; fix it and refresh".into()];
                return;
            }
        };
        let (main, workspace) = (self.config.clone(), self.workspace.clone());
        let keychain = self.relay.keychain().clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.sandbox_check = Some(rx);
        let wake = self.wake.clone();
        std::thread::spawn(move || {
            let claude = crate::sandbox::checks::claude_route(&main, workspace.as_deref(), &keychain);
            let _ = tx.send(crate::sandbox::doctor_lines(&program, &config, claude));
            wake.notify();
        });
    }

    pub(super) fn poll_sandbox_check(&mut self) -> bool {
        let Some(rx) = &self.sandbox_check else {
            return false;
        };
        match rx.try_recv() {
            Ok(lines) => self.sandbox_lines = lines,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.sandbox_lines = vec!["✗ Sandbox check stopped; refresh to retry".into()]
            }
        }
        self.sandbox_check = None;
        true
    }
}

impl Desktop<'_> {
    pub(super) fn sandbox_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("Sandbox", |ui| {
            ui.set_max_width(560.0);
            ui.label(format!("Agent isolation: {}", self.runtime.config.isolation.mode.name()));
            if self.runtime.sandbox_lines.is_empty() && self.runtime.sandbox_check.is_none() {
                self.post(|state| state.check_sandbox());
            }
            if self.runtime.sandbox_check.is_some() || self.runtime.sandbox_lines.is_empty() {
                ui.label("Checking Docker Sandboxes…");
            }
            for line in &self.runtime.sandbox_lines {
                ui.add(
                    egui::Label::new(RichText::new(line).color(if line.starts_with('✗') {
                        self.runtime.palette.warning
                    } else {
                        self.runtime.palette.foreground
                    }))
                    .wrap(),
                );
            }
            if ui
                .add_enabled(self.runtime.sandbox_check.is_none(), egui::Button::new("Refresh sandbox checks"))
                .clicked()
            {
                self.post(|state| state.check_sandbox());
            }
            ui.separator();
            ui.label("For guided fresh-machine setup, run this in a host terminal:");
            if ui.button("Copy: atomics setup").clicked() {
                ui.ctx().copy_text("atomics setup".into());
            }
            ui.label(
                "Setup reads credentials over stdin: the Claude subscription stays in the OS keychain for ATOMICS's relay, \
                 API keys go to sbx. Ticket integrations keep running on the host.",
            );
        });
    }
}
