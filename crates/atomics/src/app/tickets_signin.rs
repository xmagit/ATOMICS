//! Signing in to the ticket server from the app: per-server sign-in state, the sign-in and sign-out
//! jobs, the state the host publishes for every client (`HostInfo::sign_in`), and the desktop's
//! Tickets panel banner and header chip. Needing a sign-in is a state shown here, not an error: the
//! worker does not back off from it, and only a click (or `atomics tickets login`) ever opens a
//! browser. A ticket job whose saved sign-in the server refused marks it expired, so every place that
//! shows it offers "Reconnect"; with no saved sign-in it offers "Sign in to <system>". Once a sign-in
//! finishes, the worker connects again without a restart.
//! A client's sign-in (`host_command` `sign_in`) opens no browser on this machine: the client shows the
//! address and pastes back the one its browser ended on.
use super::*;
use crate::mcp_client::OAuth;
use crate::ticket_signin::{self, SignInJob, SignState};
use atomics_core::projection::{SignInInfo, SignInState};
use atomics_core::tickets::{McpServer, TicketConfig};

pub(super) struct SignUi {
    wake: crate::host::Wake,
    /// Keychain and browser; tests replace them.
    oauth: Option<OAuth>,
    /// The state of the configured server, and the server URL it was read for.
    state: Option<(String, SignState)>,
    pub(super) job: Option<SignInJob>,
    out: Option<std::sync::mpsc::Receiver<Result<crate::mcp_client::Revocation, String>>>,
    /// Why the last attempt failed; free of tokens.
    pub(super) failure: Option<String>,
    /// Why a ticket job said the server refused the saved sign-in (or it ran out), until a sign-in
    /// finishes, a later job gets through or the person signs out. A job that found no saved sign-in
    /// never sets it.
    pub(super) expired: Option<String>,
    /// The sign-in under way was started by a client: no browser was opened here.
    pub(super) for_client: bool,
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
            failure: None,
            expired: None,
            for_client: false,
            headless: ticket_signin::headless(),
        }
    }

    pub(super) fn oauth(&self) -> OAuth {
        self.oauth.clone().unwrap_or_else(OAuth::current)
    }

    #[cfg(test)]
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
    /// The configured server when it signs in with OAuth, with what its sign-in also asks for.
    pub(super) fn sign_server(&self) -> Option<McpServer> {
        self.tickets.config.as_ref().map(|config| config.sign_in_server()).filter(McpServer::uses_oauth)
    }

    /// A job said a sign-in is required: the banner shows, and the state is read again, since a
    /// sign-in can stop covering a resource and still be there.
    pub(super) fn ask_sign_in(&mut self) {
        self.tickets.panel.signin = true;
        self.tickets.sign.state = None;
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
            self.tickets.sign.for_client = false;
            match result {
                Ok(()) => {
                    self.tickets.sign.failure = None;
                    self.tickets.sign.expired = None;
                    self.tickets.sign.state = Some((url_of(&server), SignState::SignedIn));
                    self.tickets.panel.signin = false;
                    self.tickets.panel.focus_query = true;
                    // The worker connects again with the new sign-in, and the tickets that failed
                    // for want of it are read again.
                    self.tickets.jobs = None;
                    self.refresh_ticket_identity();
                    self.resume_ticket_outboxes();
                    self.tickets.failed.clear();
                    self.refresh_tickets(None, None);
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
            self.tickets.sign.expired = None;
            self.tickets.whoami = None;
            self.toast(message);
        }
        let checking = self.tickets.sign.job.is_none() && self.tickets.sign.out.is_none();
        if checking && self.tickets.sign.state.as_ref().is_none_or(|(url, _)| *url != url_of(&server)) {
            let state = SignState::of(&self.tickets.sign.oauth(), &server);
            if matches!(state, SignState::SignedOut | SignState::Partial) {
                self.tickets.panel.signin = true;
            }
            self.tickets.sign.state = Some((url_of(&server), state));
            changed = true;
        }
        changed
    }

    /// The desktop's Sign in (or Reconnect) button: a sign-in on its own thread, which opens this
    /// machine's browser unless it has none.
    pub(super) fn start_sign_in(&mut self) {
        let open_browser = !self.tickets.sign.headless;
        self.begin_sign_in(open_browser);
    }

    /// A client's Sign in: no browser here; the client opens the address and pastes back where its
    /// browser ended. A sign-in already under way is the one it joins.
    pub(super) fn start_client_sign_in(&mut self) -> Result<(), String> {
        if self.sign_server().is_none() {
            return Err(self.no_sign_in_reason());
        }
        if self.tickets.sign.job.is_none() {
            self.begin_sign_in(false);
            self.tickets.sign.for_client = true;
        }
        Ok(())
    }

    fn begin_sign_in(&mut self, open_browser: bool) {
        let Some(server) = self.sign_server() else { return };
        if self.tickets.sign.job.is_some() {
            return;
        }
        let sign = &mut self.tickets.sign;
        sign.failure = None;
        sign.for_client = false;
        sign.state = Some((url_of(&server), SignState::SigningIn));
        sign.job = Some(SignInJob::start(sign.oauth(), server, open_browser, &sign.wake));
        self.window.request_repaint();
    }

    /// Why there is no sign-in to start, for a client that asked for one.
    fn no_sign_in_reason(&self) -> String {
        match self.ticket_config() {
            Some(config) => format!(
                "{} does not sign in from ATOMICS: set oauth = true under [tickets.server] for that",
                config.system
            ),
            None => self.no_ticket_system(),
        }
    }

    pub(super) fn cancel_sign_in(&mut self) -> Result<(), String> {
        match &self.tickets.sign.job {
            Some(job) => {
                job.cancel();
                Ok(())
            }
            None => Err("no sign-in is under way".into()),
        }
    }

    /// The address a browser ended on after signing in (or just its query), for the sign-in under way.
    pub(super) fn paste_signin(&mut self, text: &str) -> Result<(), String> {
        let Some(job) = self.tickets.sign.job.as_mut() else {
            return Err("no sign-in is under way; start one first".into());
        };
        if text.trim().is_empty() {
            return Err("paste the address the browser ended on".into());
        }
        job.progress.rejected = None;
        job.paste(text);
        Ok(())
    }

    /// A ticket job failed: when it says a sign-in is required, every place that shows the tickets offers
    /// it, through the same `ask_sign_in` as the panel. Only when the server refused the saved sign-in
    /// (or it ran out) does the button read "Reconnect": with nothing saved, as in a new home or after
    /// signing out, it stays "Sign in to <system>". Only that signal counts: a refusal the OAuth layer
    /// already turned into an ordinary error stays one, so nothing brings the banner back by itself.
    pub(super) fn ticket_job_failed(&mut self, error: &str) {
        use crate::mcp_client::{is_sign_in_refused, is_sign_in_required, sign_in_reason};
        if self.sign_server().is_none() || !is_sign_in_required(error) {
            return;
        }
        // A refused sign-in is cleared, so the jobs after it find nothing saved; it stays expired until
        // a sign-in finishes, a job gets through or the person signs out.
        if is_sign_in_refused(error) {
            let reason = atomics_ui::text::truncate(&self.scrub(sign_in_reason(error)), 200).to_string();
            self.tickets.sign.expired = Some(reason);
        }
        self.ask_sign_in();
    }

    /// A ticket job got through: a sign-in made meanwhile (say with `atomics tickets login`) is in use.
    pub(super) fn ticket_job_succeeded(&mut self) {
        if self.tickets.sign.expired.take().is_some() {
            self.tickets.panel.signin = false;
            self.tickets.sign.state = None;
        }
    }

    /// The sign-in as every client sees it; `None` without an OAuth server or before its state is read.
    pub(crate) fn sign_in_info(&self) -> Option<SignInInfo> {
        let config = self.ticket_config()?;
        // The one sign-in, with every resource it stands for (the ticket API's too).
        let server = self.sign_server()?;
        let sign = &self.tickets.sign;
        let known = sign.state.as_ref().filter(|(url, _)| *url == url_of(&server)).map(|(_, state)| state);
        let mut info = SignInInfo {
            system: config.system.clone(),
            server: ticket_signin::host_of(&server),
            state: SignInState::SignedOut,
            reason: sign.failure.clone(),
            url: None,
            rejected: None,
            browser_failed: false,
        };
        if let Some(job) = &sign.job {
            info.state = SignInState::SigningIn;
            info.url = job.progress.url.clone();
            info.rejected = job.progress.rejected.clone();
            info.browser_failed = job.progress.browser_failed.is_some();
            return Some(info);
        }
        info.state = match known? {
            SignState::Unavailable(why) => {
                info.reason = Some(why.clone());
                SignInState::Unavailable
            }
            // Signed in without a resource the profile now reads from: one more sign-in covers it.
            SignState::Partial => {
                let hosts = ticket_signin::resource_hosts(&server);
                let lacks = format!("This sign-in does not cover {hosts}, which {} reads tickets from.", config.system);
                info.reason = info.reason.or(Some(lacks));
                SignInState::Partial
            }
            _ if sign.expired.is_some() => {
                info.reason = info.reason.or_else(|| sign.expired.clone());
                SignInState::Expired
            }
            SignState::SignedIn if sign.out.is_none() => SignInState::SignedIn,
            _ => SignInState::SignedOut,
        };
        Some(info)
    }

    /// The sign-in as this desktop's banner draws it.
    pub(super) fn sign_in_view(&self) -> Option<atomics_ui::signin::SignInView> {
        let info = self.sign_in_info()?;
        let sign = &self.tickets.sign;
        let job = sign.job.as_ref();
        Some(atomics_ui::signin::SignInView {
            info,
            // A sign-in a client started opened no browser here; then the desktop shows the steps too.
            opens_browser: !sign.headless && !(job.is_some() && sign.for_client),
            seconds_left: job.map(|job| job.patience.saturating_sub(job.started.elapsed()).as_secs()),
        })
    }

    /// Sign out: the server is told to revoke, and the sign-in is forgotten here either way.
    pub(super) fn start_sign_out(&mut self) {
        let Some(server) = self.sign_server() else { return };
        if self.tickets.sign.out.is_some() {
            return;
        }
        let _ = self.cancel_sign_in();
        self.tickets.whoami = None;
        self.tickets.identity_epoch += 1;
        self.tickets.jobs = None;
        let sign = &mut self.tickets.sign;
        sign.out = Some(ticket_signin::sign_out(sign.oauth(), server, &sign.wake));
        // Until the sign-out is done the panel asks to sign in; nothing is read in the meantime.
        self.tickets.panel.signin = true;
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
    /// The panel while the server needs a sign-in (or a sign-in is under way): the shared banner, as the
    /// board shows it in every client.
    pub(super) fn signin_banner(&mut self, ui: &mut egui::Ui, config: &TicketConfig, p: &Palette) {
        ui.add_space(10.0);
        if !config.server.uses_oauth() {
            ui.label(RichText::new(format!("{} is not signed in", config.system)).strong().color(p.warning));
            ui.label("Set oauth = true under [tickets.server] to sign in with the browser from here.");
            if ui.button("Try again").clicked() {
                self.runtime.tickets.panel.signin = false;
            }
            return;
        }
        // A sign-in that does not cover every resource the profile reads from asks for one more there too;
        // what it does cover keeps working, so the banner may be put away, and its tokens signed out.
        let partial = self.runtime.tickets.sign.state() == Some(&SignState::Partial);
        let drew = self.draw(|backend, client| atomics_ui::signin::banner(ui, backend, client));
        if (!drew || partial) && ui.button("Try again").clicked() {
            self.runtime.tickets.panel.signin = false;
        }
        // The chip is for a sign-in that covers everything; this one still holds tokens to forget.
        if partial && ui.button("Sign out").clicked() {
            self.runtime.start_sign_out();
        }
    }

    /// The header chip: signed in, with the way out.
    pub(super) fn signed_in_chip(&mut self, ui: &mut egui::Ui, p: &Palette, system: &str) {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Sign out").clicked() {
                self.runtime.start_sign_out();
            }
            ui.label(
                RichText::new(
                    self.runtime
                        .ticket_identity_label()
                        .map(|label| format!("{system} · {label}"))
                        .unwrap_or_else(|| format!("{system} · signed in")),
                )
                .size(11.0)
                .color(p.muted),
            );
            ui.label(RichText::new("●").size(10.5).color(p.success));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::with_app;
    use crate::mcp_client::FakeOAuthServer;
    use atomics_core::ops::{HostAction, HostCommand, Outcome};

    fn pump(app: &mut Desktop<'_>, until: impl Fn(&State) -> bool) {
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(20));
        while !until(app.runtime) {
            app.runtime.poll_signin();
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Pump the ticket worker and the sign-in until `until` holds.
    fn work(app: &mut Desktop<'_>, what: &str, until: impl Fn(&State) -> bool) {
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(30));
        while !until(app.runtime) {
            app.runtime.ticket_results();
            app.runtime.poll_signin();
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn host_command(app: &mut Desktop<'_>, op: &str, action: HostAction) -> Outcome {
        app.runtime.host_command(HostCommand { op: op.into(), action }).map_or_else(Outcome::refused, Outcome::done)
    }

    /// The button, and the state it is drawn from.
    fn button(app: &Desktop<'_>) -> (SignInState, String) {
        let info = app.runtime.sign_in_info().expect("an OAuth server");
        (info.state, atomics_ui::signin::button_label(&info))
    }

    /// Signs in through the fake server's browser, and waits for the identity the worker reads after.
    fn sign_in_here(app: &mut Desktop<'_>) {
        app.runtime.start_sign_in();
        work(app, "the address", |r| r.sign_in_info().and_then(|i| i.url).is_some());
        crate::mcp_client::oauth_test_browse(&app.runtime.sign_in_info().unwrap().url.unwrap());
        work(app, "the sign-in", |r| r.sign_in_info().is_some_and(|i| i.state == SignInState::SignedIn));
        work(app, "the identity", |r| r.tickets.whoami.is_some());
    }

    /// Runs the whoami job and waits for its answer, then reads the sign-in state again.
    fn whoami_answered(app: &mut Desktop<'_>, what: &str) {
        app.runtime.refresh_ticket_identity();
        assert!(app.runtime.tickets.pending > 0, "the whoami job is on its way");
        work(app, what, |r| r.tickets.pending == 0);
        app.runtime.poll_signin();
    }

    #[test]
    fn reconnect_shows_only_once_the_server_refuses_a_saved_sign_in() {
        with_app(
            "app::tickets_signin::tests::reconnect_shows_only_once_the_server_refuses_a_saved_sign_in",
            |app, _| {
                let fake = FakeOAuthServer::start();
                let (_dir, oauth) = crate::mcp_client::oauth_test_machine();
                app.runtime.config.tickets = toml::from_str(
                "system = \"tracker\"\n[operations.whoami]\ntool = \"whoami\"\nresult = { id = \"userId\", kind = \"kind\", name = \"displayName\" }\n",
            )
            .unwrap();
                app.runtime.config.tickets.server = fake.server();
                app.runtime.tickets.sign.override_oauth(oauth);
                app.runtime.tickets.sign.headless = true;
                let sign_in = (SignInState::SignedOut, "Sign in to tracker".to_string());

                // A fresh home: the whoami job sent as the config applies finds no saved sign-in. The
                // panel asks for one, and the button signs in rather than reconnects.
                app.runtime.apply_ticket_config();
                assert!(app.runtime.tickets.pending > 0, "the whoami job is on its way");
                work(app, "the first whoami job", |r| r.tickets.pending == 0);
                app.runtime.poll_signin();
                assert!(app.runtime.tickets.panel.signin);
                assert_eq!(button(app), sign_in);
                assert!(app.runtime.tickets.sign.expired.is_none());
                assert_eq!(app.runtime.host_info().sign_in.map(|info| info.state), Some(SignInState::SignedOut));

                // Signed in, then out: the jobs after it find nothing saved either.
                sign_in_here(app);
                app.runtime.start_sign_out();
                work(app, "the sign-out", |r| {
                    r.tickets.sign.out.is_none() && r.tickets.sign.state() == Some(&SignState::SignedOut)
                });
                whoami_answered(app, "the whoami job after signing out");
                assert!(app.runtime.tickets.panel.signin);
                assert_eq!(button(app), sign_in);

                // Signed in again, and the server refuses the saved sign-in: now it is Reconnect, saying why.
                sign_in_here(app);
                {
                    let mut st = fake.state.lock().unwrap();
                    st.access.clear();
                    st.refresh.clear();
                }
                whoami_answered(app, "the refusal");
                assert_eq!(button(app), (SignInState::Expired, "Reconnect".to_string()));
                let info = app.runtime.sign_in_info().unwrap();
                assert!(info.reason.as_deref().is_some_and(|r| r.contains("refused the saved sign-in")), "{info:?}");
                // The refusal cleared the saved sign-in, so the next job finds none; it is still Reconnect.
                whoami_answered(app, "a job after the refusal");
                assert_eq!(button(app), (SignInState::Expired, "Reconnect".to_string()));
                // Signing out from there forgets the refusal too.
                app.runtime.start_sign_out();
                work(app, "the second sign-out", |r| {
                    r.tickets.sign.out.is_none() && r.tickets.sign.state() == Some(&SignState::SignedOut)
                });
                whoami_answered(app, "the whoami job after the second sign-out");
                assert_eq!(button(app), sign_in);
            },
        );
    }

    #[test]
    fn a_refused_sign_in_offers_reconnect_and_a_clients_sign_in_reconnects_the_worker() {
        with_app(
            "app::tickets_signin::tests::a_refused_sign_in_offers_reconnect_and_a_clients_sign_in_reconnects_the_worker",
            |app, _| {
                let fake = FakeOAuthServer::start();
                let (_dir, oauth) = crate::mcp_client::oauth_test_machine();
                app.runtime.config.tickets = toml::from_str(
                    "system = \"tracker\"\n[operations.whoami]\ntool = \"whoami\"\nresult = { id = \"userId\", kind = \"kind\", name = \"displayName\" }\n",
                )
                .unwrap();
                app.runtime.config.tickets.server = fake.server();
                app.runtime.tickets.sign.override_oauth(oauth);
                app.runtime.tickets.sign.headless = false;
                app.runtime.apply_ticket_config();
                app.runtime.poll_signin();
                let info = app.runtime.sign_in_info().unwrap();
                assert_eq!((info.state, info.system.as_str()), (SignInState::SignedOut, "tracker"));
                assert_eq!(atomics_ui::signin::button_label(&info), "Sign in to tracker");
                assert_eq!(app.runtime.host_info().sign_in, Some(info));

                // A client's sign-in opens no browser here: the address is the client's to open, and
                // a wrong address pasted back is refused while the sign-in keeps waiting.
                assert!(matches!(host_command(app, "o1", HostAction::SignIn), Outcome::Done { .. }));
                assert!(app.runtime.tickets.sign.for_client);
                work(app, "the address", |r| r.sign_in_info().and_then(|i| i.url).is_some());
                let view = app.runtime.sign_in_view().unwrap();
                assert!(!view.opens_browser, "the desktop shows the steps for a client's sign-in too");
                assert_eq!(view.info.state, SignInState::SigningIn);
                let url = view.info.url.unwrap();
                let wrong = HostAction::SignInPaste { text: "http://127.0.0.1:1/callback?code=x&state=wrong".into() };
                assert!(matches!(host_command(app, "o2", wrong), Outcome::Done { .. }));
                work(app, "the refusal", |r| r.sign_in_info().and_then(|i| i.rejected).is_some());
                crate::mcp_client::oauth_test_browse(&url);
                work(app, "the sign-in", |r| r.sign_in_info().is_some_and(|i| i.state == SignInState::SignedIn));
                work(app, "the identity", |r| r.tickets.whoami.is_some());
                let first = fake.state.lock().unwrap().call_auth.last().cloned().unwrap();

                // The server forgets the sign-in (it ran out, or was revoked): the next job is refused,
                // and every place that shows the tickets offers Reconnect, saying why.
                {
                    let mut st = fake.state.lock().unwrap();
                    st.access.clear();
                    st.refresh.clear();
                }
                app.runtime.refresh_ticket_identity();
                work(app, "the refusal to show", |r| r.sign_in_info().is_some_and(|i| i.state == SignInState::Expired));
                let info = app.runtime.sign_in_info().unwrap();
                assert!(info.reason.as_deref().is_some_and(|r| r.contains("refused the saved sign-in")), "{info:?}");
                assert_eq!(atomics_ui::signin::button_label(&info), "Reconnect");
                assert!(app.runtime.tickets.panel.signin);

                // Reconnect signs in again, and the worker connects with the new sign-in: no restart.
                assert!(matches!(host_command(app, "o3", HostAction::SignIn), Outcome::Done { .. }));
                work(app, "the second address", |r| r.sign_in_info().and_then(|i| i.url).is_some());
                crate::mcp_client::oauth_test_browse(&app.runtime.sign_in_info().unwrap().url.unwrap());
                work(app, "the second sign-in", |r| r.sign_in_info().is_some_and(|i| i.state == SignInState::SignedIn));
                assert!(app.runtime.tickets.sign.expired.is_none() && !app.runtime.tickets.panel.signin);
                work(app, "the identity again", |r| r.tickets.whoami.is_some());
                let st = fake.state.lock().unwrap();
                let second = st.call_auth.last().cloned().unwrap();
                assert!(second != first && st.access.contains(&second), "the worker uses the new sign-in");
                drop(st);

                // Nothing to paste into, or cancel, once it is done; the client is told why.
                let paste = host_command(app, "o4", HostAction::SignInPaste { text: "x".into() });
                assert!(
                    matches!(&paste, Outcome::Refused { message, .. } if message.contains("no sign-in is under way"))
                );
                assert!(matches!(host_command(app, "o5", HostAction::SignInCancel), Outcome::Refused { .. }));
            },
        );
    }

    #[test]
    fn only_a_required_sign_in_offers_reconnect_and_a_job_that_gets_through_ends_it() {
        with_app(
            "app::tickets_signin::tests::only_a_required_sign_in_offers_reconnect_and_a_job_that_gets_through_ends_it",
            |app, _| {
                let fake = FakeOAuthServer::start();
                let (_dir, oauth) = crate::mcp_client::oauth_test_machine();
                app.runtime.config.tickets = toml::from_str("system = \"tracker\"").unwrap();
                app.runtime.config.tickets.server = fake.server();
                app.runtime.tickets.sign.override_oauth(oauth);
                app.runtime.apply_ticket_config();
                let signed_in = |app: &mut Desktop<'_>| {
                    app.runtime.tickets.sign.state = Some((url_of(&fake.server()), SignState::SignedIn));
                    app.runtime.tickets.panel.signin = false;
                };
                signed_in(app);
                assert_eq!(app.runtime.sign_in_info().unwrap().state, SignInState::SignedIn);
                // An ordinary failure is not about the sign-in, whatever its words: the OAuth layer says
                // `sign-in required` when one would help, and only once for a resource that keeps refusing.
                for error in [
                    "reading the ticket failed: ENG-123 was not found",
                    "reading the ticket failed: tool error: needs-refresh",
                    "reading the ticket failed: the API refused the sign-in again; signing in again did not change it",
                ] {
                    app.runtime.ticket_job_failed(error);
                    assert_eq!(app.runtime.sign_in_info().unwrap().state, SignInState::SignedIn, "{error}");
                    assert!(!app.runtime.tickets.panel.signin, "{error}");
                }
                // A required sign-in asks, through the panel's own `ask_sign_in`, and the state is read again.
                // With nothing saved it is a sign-in, not a reconnect.
                app.runtime.ticket_job_failed("sign-in required: https://tracker.example/mcp needs a sign-in");
                assert!(app.runtime.tickets.panel.signin && app.runtime.tickets.sign.state.is_none());
                app.runtime.poll_signin();
                let info = app.runtime.sign_in_info().unwrap();
                assert_eq!((info.state, info.reason.as_deref()), (SignInState::SignedOut, None));
                assert_eq!(atomics_ui::signin::button_label(&info), "Sign in to tracker");
                // The server refused the saved one: Reconnect, saying why.
                signed_in(app);
                app.runtime.ticket_job_failed("sign-in required again: the server refused the saved sign-in");
                assert!(app.runtime.tickets.panel.signin && app.runtime.tickets.sign.state.is_none());
                app.runtime.poll_signin();
                let info = app.runtime.sign_in_info().unwrap();
                assert_eq!(info.state, SignInState::Expired);
                assert_eq!(info.reason.as_deref(), Some("the server refused the saved sign-in"));
                // The refused sign-in was cleared, so the next job finds none: still Reconnect.
                app.runtime.ticket_job_failed("sign-in required: https://tracker.example/mcp needs a sign-in");
                app.runtime.poll_signin();
                assert_eq!(app.runtime.sign_in_info().unwrap().state, SignInState::Expired);
                // A sign-in made elsewhere (`atomics tickets login`) is noticed by the next job that gets through.
                app.runtime.ticket_job_succeeded();
                signed_in(app);
                assert_eq!(app.runtime.sign_in_info().unwrap().state, SignInState::SignedIn);
            },
        );
    }

    #[test]
    fn a_sign_in_that_does_not_cover_the_ticket_api_asks_for_one_more_naming_it() {
        with_app(
            "app::tickets_signin::tests::a_sign_in_that_does_not_cover_the_ticket_api_asks_for_one_more_naming_it",
            |app, _| {
                let fake = FakeOAuthServer::start();
                let (_dir, oauth) = crate::mcp_client::oauth_test_machine();
                app.runtime.config.tickets = toml::from_str(
                    "system = \"tracker\"\n[operations.read]\nhttp = { method = \"GET\", path = \"/items/{id}\" }\nresult = { id = \"id\" }",
                )
                .unwrap();
                app.runtime.config.tickets.server = fake.server();
                app.runtime.config.tickets.http.url = fake.api_url.clone();
                app.runtime.tickets.sign.override_oauth(oauth);
                app.runtime.apply_ticket_config();
                // The one sign-in stands for the server and the API: the shared state says so.
                let server = app.runtime.sign_server().unwrap();
                assert!(!server.resources.is_empty(), "{server:?}");
                app.runtime.tickets.sign.state = Some((url_of(&server), SignState::Partial));
                let info = app.runtime.sign_in_info().unwrap();
                assert_eq!(info.state, SignInState::Partial);
                assert!(info.needs_sign_in());
                assert_eq!(atomics_ui::signin::button_label(&info), "Sign in to tracker");
                let hosts = ticket_signin::resource_hosts(&server);
                assert!(info.reason.as_deref().is_some_and(|r| r.contains(&hosts)), "{info:?}");

                // A client's sign-in is the same one: it asks for the server and the API together.
                assert!(matches!(host_command(app, "o1", HostAction::SignIn), Outcome::Done { .. }));
                work(app, "the address", |r| r.sign_in_info().and_then(|i| i.url).is_some());
                let url = app.runtime.sign_in_info().unwrap().url.unwrap();
                let resources =
                    url::Url::parse(&url).unwrap().query_pairs().filter(|(key, _)| key == "resource").count();
                assert_eq!(resources, 2, "{url}");
                app.runtime.cancel_sign_in().unwrap();
            },
        );
    }

    #[test]
    fn ticket_signin_state_banner_and_sign_out_follow_the_fake_server() {
        with_app(
            "app::tickets_signin::tests::ticket_signin_state_banner_and_sign_out_follow_the_fake_server",
            |app, _| {
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
                app.runtime.paste_signin("http://127.0.0.1:1/callback?code=x&state=wrong").unwrap();
                pump(app, |r| r.tickets.sign.job.as_ref().is_some_and(|j| j.progress.rejected.is_some()));
                crate::mcp_client::oauth_test_browse(&url);
                pump(app, |r| r.tickets.sign.job.is_none());
                assert_eq!(app.runtime.tickets.sign.state(), Some(&SignState::SignedIn));
                assert!(!app.runtime.tickets.panel.signin);

                app.runtime.start_sign_out();
                assert!(app.runtime.tickets.panel.signin);
                pump(app, |r| r.tickets.sign.out.is_none() && r.tickets.sign.state() == Some(&SignState::SignedOut));
                assert_eq!(fake.state.lock().unwrap().revoked.len(), 2);
            },
        );
    }
}
