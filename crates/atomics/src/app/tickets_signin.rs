//! Signing in to the ticket server from the app: per-server sign-in state, the sign-in and sign-out
//! jobs, the banner with its headless paste box, and the header chip. Needing a sign-in is a state
//! shown here, not an error: the worker does not back off from it, and only a click (or
//! `atomics tickets login`) ever opens a browser.
use super::*;
use crate::mcp_client::OAuth;
use crate::ticket_signin::{self, SignInJob, SignState};
use atomics_core::tickets::{McpServer, TicketConfig};

pub(super) struct SignUi {
    wake: crate::host::Wake,
    /// Keychain and browser; tests replace them.
    oauth: Option<OAuth>,
    /// The state of the configured server, and the server URL it was read for.
    state: Option<(String, SignState)>,
    pub(super) job: Option<SignInJob>,
    out: Option<std::sync::mpsc::Receiver<Result<crate::mcp_client::Revocation, String>>>,
    /// The address being pasted, and whether the manual steps show on a machine with a browser.
    pub(super) paste: String,
    pub(super) manual: bool,
    /// Why the last attempt failed; free of tokens.
    pub(super) failure: Option<String>,
    /// Whether the machine has no browser. Tests set it.
    pub(super) headless: bool,
}

impl SignUi {
    pub(super) fn new(wake: &crate::host::Wake) -> SignUi {
        SignUi {
            wake: wake.clone(),
            oauth: None,
            state: None,
            job: None,
            out: None,
            paste: String::new(),
            manual: false,
            failure: None,
            headless: ticket_signin::headless(),
        }
    }

    pub(super) fn oauth(&self) -> OAuth {
        self.oauth.clone().unwrap_or_else(OAuth::current)
    }

    pub(super) fn override_oauth(&mut self, oauth: OAuth) {
        self.oauth = Some(oauth);
        self.state = None;
    }

    pub(super) fn override_oauth_ref(&self) -> Option<OAuth> {
        self.oauth.clone()
    }

    pub(super) fn state(&self) -> Option<&SignState> {
        self.state.as_ref().map(|(_, state)| state)
    }
}

fn url_of(server: &McpServer) -> String {
    server.url.clone().unwrap_or_default()
}

impl State {
    /// The configured server when it signs in with OAuth.
    pub(super) fn sign_server(&self) -> Option<McpServer> {
        self.tickets.config.as_ref().map(|c| c.server.clone()).filter(McpServer::uses_oauth)
    }

    /// Reads the sign-in state once per server, and collects a finished sign-in or sign-out. Called
    /// every tick; returns whether something changed.
    pub(super) fn poll_signin(&mut self) -> bool {
        let Some(server) = self.sign_server() else { return false };
        let mut changed = false;
        let host = ticket_signin::host_of(&server);
        if let Some(job) = self.tickets.sign.job.as_mut()
            && let Some(result) = job.poll()
        {
            changed = true;
            self.tickets.sign.job = None;
            self.tickets.sign.paste.clear();
            match result {
                Ok(()) => {
                    self.tickets.sign.failure = None;
                    self.tickets.sign.state = Some((url_of(&server), SignState::SignedIn));
                    self.tickets.panel.signin = false;
                    self.tickets.panel.focus_query = true;
                    // The worker connects again with the new sign-in.
                    self.tickets.jobs = None;
                    self.toast(format!("Signed in to {host}"));
                }
                Err(error) => {
                    let error = self.scrub(&error);
                    self.tickets.sign.failure = Some(error.clone());
                    self.tickets.sign.state = None;
                    self.toast(format!("Sign-in to {host} did not finish: {error}"));
                }
            }
        }
        if let Some(rx) = &self.tickets.sign.out
            && let Ok(result) = rx.try_recv()
        {
            changed = true;
            self.tickets.sign.out = None;
            let message = ticket_signin::sign_out_message(&host, &result);
            let message = self.scrub(&message);
            self.tickets.sign.state = None;
            self.toast(message);
        }
        let checking = self.tickets.sign.job.is_none() && self.tickets.sign.out.is_none();
        if checking && self.tickets.sign.state.as_ref().is_none_or(|(url, _)| *url != url_of(&server)) {
            let state = SignState::of(&self.tickets.sign.oauth(), &server);
            if state == SignState::SignedOut {
                self.tickets.panel.signin = true;
            }
            self.tickets.sign.state = Some((url_of(&server), state));
            changed = true;
        }
        changed
    }

    /// The Sign in button: a browser sign-in on its own thread.
    pub(super) fn start_sign_in(&mut self) {
        let Some(server) = self.sign_server() else { return };
        if self.tickets.sign.job.is_some() {
            return;
        }
        let sign = &mut self.tickets.sign;
        sign.failure = None;
        sign.manual = false;
        sign.paste.clear();
        sign.state = Some((url_of(&server), SignState::SigningIn));
        sign.job = Some(SignInJob::start(sign.oauth(), server, !sign.headless, &sign.wake));
        self.ctx.request_repaint();
    }

    pub(super) fn cancel_sign_in(&mut self) {
        if let Some(job) = &self.tickets.sign.job {
            job.cancel();
        }
    }

    /// The address pasted back from a browser on another machine.
    pub(super) fn paste_signin(&mut self) {
        let text = std::mem::take(&mut self.tickets.sign.paste);
        if let Some(job) = self.tickets.sign.job.as_mut().filter(|_| !text.trim().is_empty()) {
            job.progress.rejected = None;
            job.paste(&text);
        }
    }

    /// Sign out: the server is told to revoke, and the sign-in is forgotten here either way.
    pub(super) fn start_sign_out(&mut self) {
        let Some(server) = self.sign_server() else { return };
        if self.tickets.sign.out.is_some() {
            return;
        }
        self.cancel_sign_in();
        self.tickets.jobs = None;
        let sign = &mut self.tickets.sign;
        sign.out = Some(ticket_signin::sign_out(sign.oauth(), server, &sign.wake));
        // Until the sign-out is done the panel asks to sign in; nothing is read in the meantime.
        self.tickets.panel.signin = true;
    }

    /// The Actions entry: opens the panel on the banner and starts the browser sign-in.
    pub(super) fn show_signin(&mut self) {
        self.panel = PanelMode::Tickets;
        self.diff_open = true;
        self.tickets.panel.signin = true;
        self.start_sign_in();
    }

    /// Lets sandboxed agents use the ticket server's own tools: the existing Connect, signing in with sbx.
    pub(super) fn connect_ticket_server(&mut self) {
        let Some(config) = self.ticket_config() else { return };
        let url = config.server.url.clone().unwrap_or_default();
        let name = atomics_core::mcp_import::sbx_name(&config.system);
        match name.map(|name| self.add_connection(&name, &url)) {
            Some(Ok(message)) => self.toast(message),
            Some(Err(error)) => self.toast(error),
            None => self.toast("Connect it from Connections: the system name is not a connection name"),
        }
    }
}

impl Desktop<'_> {
    /// The panel while the server needs a sign-in (or a sign-in is under way).
    pub(super) fn signin_banner(&mut self, ui: &mut egui::Ui, config: &TicketConfig, p: &Palette) {
        let host = ticket_signin::host_of(&config.server);
        ui.add_space(10.0);
        if !config.server.uses_oauth() {
            ui.label(RichText::new(format!("{} needs a sign-in", config.system)).strong().color(p.warning));
            ui.label("Set oauth = true under [tickets.server] to sign in with the browser from here.");
            if ui.small_button("Try again").clicked() {
                self.runtime.tickets.panel.signin = false;
            }
            return;
        }
        if self.runtime.tickets.sign.job.is_some() {
            self.signing_in(ui, p);
            return;
        }
        ui.label(RichText::new(format!("Sign in to {} to search, read and plan from its tickets.", config.system)).strong());
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            if ui.button("Sign in with the browser").clicked() {
                self.runtime.start_sign_in();
            }
            ui.label(
                RichText::new(
                    "Opens your browser. ATOMICS keeps the sign-in in the OS keychain, never in a file, and the \
                     agents never see it.",
                )
                .color(p.muted),
            );
        });
        if let Some(failure) = self.runtime.tickets.sign.failure.clone() {
            ui.add_space(4.0);
            ui.label(RichText::new(failure).color(p.warning));
        }
        ui.add_space(6.0);
        ui.label(RichText::new(format!("system: {}  ·  {host}", config.system)).size(11.5).color(p.muted));
        if self.runtime.tickets.sign.state() == Some(&SignState::SignedIn) && ui.small_button("Try again").clicked() {
            self.runtime.tickets.panel.signin = false;
        }
    }

    fn signing_in(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.ctx().request_repaint_after(Duration::from_millis(500));
        let sign = &mut self.runtime.tickets.sign;
        let Some(job) = sign.job.as_ref() else { return };
        let left = job.patience.saturating_sub(job.started.elapsed()).as_secs();
        let manual = sign.headless || job.progress.browser_failed.is_some() || sign.manual;
        let (url, failed, rejected) =
            (job.progress.url.clone(), job.progress.browser_failed.is_some(), job.progress.rejected.clone());
        let (mut cancel, mut copy, mut use_it, mut show) = (false, false, false, false);
        ui.horizontal(|ui| {
            ui.label(if sign.headless { "Waiting for the sign-in…" } else { "Waiting for the browser…" });
            if ui.button("Cancel").clicked() {
                cancel = true;
            }
            ui.label(RichText::new(format!("({}:{:02})", left / 60, left % 60)).color(p.muted));
        });
        if !manual {
            if ui.small_button("No browser here?").clicked() {
                show = true;
            }
        } else {
            ui.add_space(4.0);
            if failed {
                ui.label(RichText::new("The browser could not be opened.").color(p.warning));
            }
            ui.label("Open this link on any machine, signed in as you:");
            match &url {
                Some(url) => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Label::new(RichText::new(url).monospace().size(11.0)).truncate());
                        if ui.small_button("Copy link").clicked() {
                            copy = true;
                        }
                    });
                }
                None => {
                    ui.label(RichText::new("Preparing the link…").color(p.muted));
                }
            }
            ui.label("Then paste the address the browser ends on (it starts with http://127.0.0.1):");
            ui.horizontal(|ui| {
                let edit = egui::TextEdit::singleline(&mut sign.paste)
                    .hint_text("http://127.0.0.1:…/callback?code=…&state=…")
                    .desired_width(ui.available_width() - 70.0);
                let response = ui.add(edit);
                let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.button("Use it").clicked() || enter {
                    use_it = true;
                }
            });
            if let Some(why) = rejected {
                ui.label(RichText::new(why).color(p.warning));
            }
        }
        if show {
            sign.manual = true;
        }
        if copy && let Some(url) = url {
            ui.ctx().copy_text(url);
        }
        if cancel {
            self.runtime.cancel_sign_in();
        }
        if use_it {
            self.runtime.paste_signin();
        }
    }

    /// The header chip: signed in, with the way out.
    pub(super) fn signed_in_chip(&mut self, ui: &mut egui::Ui, p: &Palette, system: &str) {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("Sign out").clicked() {
                self.runtime.start_sign_out();
            }
            ui.label(RichText::new(format!("{system} · signed in")).size(11.5).color(p.muted));
            ui.label(RichText::new("●").size(10.0).color(p.success));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::with_app;
    use crate::mcp_client::FakeOAuthServer;

    fn pump(app: &mut Desktop<'_>, until: impl Fn(&State) -> bool) {
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(20));
        while !until(&app.runtime) {
            app.runtime.poll_signin();
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn ticket_signin_state_banner_and_sign_out_follow_the_fake_server() {
        with_app("app::tickets_signin::tests::ticket_signin_state_banner_and_sign_out_follow_the_fake_server", |app, _| {
            let fake = FakeOAuthServer::start();
            let (_dir, oauth) = crate::mcp_client::oauth_test_machine();
            app.runtime.config.tickets = toml::from_str("system = \"echo\"").unwrap();
            app.runtime.config.tickets.server = fake.server();
            app.runtime.apply_ticket_config();
            app.runtime.tickets.sign.override_oauth(oauth);
            app.runtime.tickets.sign.headless = true;

            // Not signed in is a state: the banner shows and nothing was tried.
            assert!(!app.runtime.tickets.panel.signin);
            app.runtime.poll_signin();
            assert!(app.runtime.tickets.panel.signin);
            assert_eq!(app.runtime.tickets.sign.state(), Some(&SignState::SignedOut));

            app.runtime.start_sign_in();
            assert_eq!(app.runtime.tickets.sign.state(), Some(&SignState::SigningIn));
            let url = loop {
                app.runtime.poll_signin();
                if let Some(url) = app.runtime.tickets.sign.job.as_ref().and_then(|j| j.progress.url.clone()) {
                    break url;
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            // The wrong address is refused and the sign-in keeps waiting; the right one finishes it.
            app.runtime.tickets.sign.paste = "http://127.0.0.1:1/callback?code=x&state=wrong".into();
            app.runtime.paste_signin();
            pump(app, |r| r.tickets.sign.job.as_ref().is_some_and(|j| j.progress.rejected.is_some()));
            crate::mcp_client::oauth_test_browse(&url);
            pump(app, |r| r.tickets.sign.job.is_none());
            assert_eq!(app.runtime.tickets.sign.state(), Some(&SignState::SignedIn));
            assert!(!app.runtime.tickets.panel.signin);

            app.runtime.start_sign_out();
            assert!(app.runtime.tickets.panel.signin);
            pump(app, |r| r.tickets.sign.out.is_none() && r.tickets.sign.state() == Some(&SignState::SignedOut));
            assert_eq!(fake.state.lock().unwrap().revoked.len(), 2);
        });
    }
}
