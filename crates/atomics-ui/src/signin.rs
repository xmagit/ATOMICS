//! The ticket system's sign-in, wherever needing one shows: an obvious "Sign in to <system>" (or, for a
//! sign-in the server refused or that ran out, "Reconnect") that runs the host's sign-in job, and while it
//! waits, the address to open and a box for the address the browser ends on. The desktop's host may open
//! its own browser; a browser client opens the address in a tab and pastes back where it ended, since the
//! sign-in listens on the host's loopback. Once it finishes, the host's ticket worker connects again.
//!
//! The paste draft and whether the manual steps show are the client's own ([`Client::sign_in_paste`],
//! [`Client::sign_in_manual`]); the sign-in's state is the host's ([`Backend::sign_in`]).

use atomics_core::projection::{SignInInfo, SignInState};
use egui::RichText;

use crate::touch;
use crate::{Backend, Client, Command};

/// The sign-in as one screen shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignInView {
    pub info: SignInInfo,
    /// A sign-in this screen starts opens a browser on its own machine; otherwise its address is opened by
    /// hand and the address the browser ends on is pasted back.
    pub opens_browser: bool,
    /// How long the sign-in under way still waits, in seconds, where the backend can tell.
    pub seconds_left: Option<u64>,
}

/// What the button that starts a sign-in says.
pub fn button_label(info: &SignInInfo) -> String {
    match info.state {
        SignInState::Expired => "Reconnect".into(),
        _ => format!("Sign in to {}", info.system),
    }
}

/// Whether [`banner`] draws anything: the ticket system needs a sign-in, or one is under way.
pub fn shows(backend: &dyn Backend) -> bool {
    backend.sign_in().is_some_and(|view| !matches!(view.info.state, SignInState::SignedIn | SignInState::Unknown))
}

/// The top bar's item while the ticket system needs a sign-in, which a platform adds through its
/// `top_bar_items` hook: "Sign in to <system>" or "Reconnect" in the warning colour, which starts the
/// sign-in; "Signing in…" while one is under way. Its popup holds the [`banner`], so the link and the
/// paste box show wherever the item is, even with no board or sidebar on screen (in the Actions menu it
/// is a submenu); a click also opens where the platform shows them beside the tickets (the Tickets
/// panel where it has one, else an open plan's board). Nothing is drawn while signed in, or where the
/// backend cannot start a sign-in.
pub fn top_bar_item(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let Some(view) = backend.sign_in() else { return };
    if !backend.available(&Command::SignInTickets) {
        return;
    }
    let p = client.palette.clone();
    let info = &view.info;
    let (text, color) = match info.state {
        SignInState::SignedOut | SignInState::Expired | SignInState::Partial | SignInState::Unavailable => {
            (button_label(info), p.warning)
        }
        SignInState::SigningIn => ("Signing in…".to_string(), p.muted),
        SignInState::SignedIn | SignInState::Unknown => return,
    };
    // The popup stays open while the paste box is clicked and typed into; a click outside closes it.
    use egui::containers::menu::{MenuButton, MenuConfig, SubMenuButton, is_in_menu};
    let config = MenuConfig::new().close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside);
    let label = RichText::new(text).color(color);
    let steps = |ui: &mut egui::Ui| {
        ui.set_min_width(360.0);
        banner(ui, backend, client);
    };
    let (response, _) = if is_in_menu(ui) {
        SubMenuButton::new(label).config(config).ui(ui, steps)
    } else {
        MenuButton::new(label).config(config).ui(ui, steps)
    };
    crate::topbar::record_item(ui, "sign-in", &response);
    let hover = match (&info.reason, info.state) {
        (_, SignInState::SigningIn) => format!("Signing in to {}: the steps show with the tickets", info.system),
        (Some(reason), _) => format!("{} needs a sign-in: {reason}", info.system),
        (None, _) => format!("Sign in to {} to search, read and plan from its tickets", info.system),
    };
    if !response.on_hover_text(hover).clicked() {
        return;
    }
    if info.state != SignInState::SigningIn {
        client.sign_in_paste.clear();
        client.send(Command::SignInTickets);
    }
    // Show where the link and the paste box are.
    let tickets = Command::SetPanel(crate::PanelMode::Tickets);
    let board = Command::SetPanel(crate::PanelMode::Plan);
    if backend.available(&tickets) {
        client.send(tickets);
        client.send(Command::OpenDiff);
    } else if !no_board_shows(backend) && backend.available(&board) {
        client.send(board);
        client.send(Command::OpenDiff);
    }
}

/// Whether no open plan's board can show the banner, so the sidebar and an empty board show it instead.
pub fn no_board_shows(backend: &dyn Backend) -> bool {
    !backend.plan_list().iter().any(|plan| plan.status == atomics_core::plan::PlanStatus::Open)
}

/// [`banner`] in a frame of the warning colour, as the board shows it above its tickets.
pub fn framed(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    if !shows(backend) {
        return;
    }
    let p = client.palette.clone();
    egui::Frame::new()
        .fill(crate::theme::lerp(p.raised, p.warning, 0.08))
        .stroke(egui::Stroke::new(1.0, p.warning.gamma_multiply(0.5)))
        .corner_radius(6)
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            banner(ui, backend, client);
        });
    ui.add_space(4.0);
}

/// The banner while the ticket system needs a sign-in or one is under way; nothing while it is signed in.
/// Returns whether it drew anything.
pub fn banner(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) -> bool {
    let Some(view) = backend.sign_in() else { return false };
    let info = &view.info;
    let p = client.palette.clone();
    let touch = client.touch;
    let can = backend.available(&Command::SignInTickets);
    match info.state {
        SignInState::SignedIn | SignInState::Unknown => return false,
        SignInState::SigningIn => {
            signing_in(ui, &view, can, client);
            return true;
        }
        SignInState::SignedOut | SignInState::Expired | SignInState::Partial | SignInState::Unavailable => {}
    }
    client.sign_in_manual = false;
    let headline = match info.state {
        SignInState::Expired => format!("{} needs you to sign in again", info.system),
        SignInState::Partial => format!("{} needs one more sign-in", info.system),
        SignInState::Unavailable => format!("{}: the sign-in could not be read", info.system),
        _ => format!("{} is not signed in", info.system),
    };
    ui.label(RichText::new(headline).strong().color(p.warning));
    if let Some(reason) = &info.reason {
        ui.label(RichText::new(reason).color(p.muted).size(11.0));
    }
    let row = |ui: &mut egui::Ui| {
        if !can {
            ui.label(
                RichText::new(format!(
                    "This screen cannot start the sign-in now (offline, or view-only); sign in to {} from \
                     ATOMICS on the computer, or from a screen that may change this host.",
                    info.system
                ))
                .color(p.muted),
            );
            return;
        }
        let label = button_label(info);
        let hover = if info.state == SignInState::Expired {
            format!("Sign in to {} again; the tickets connect with the new sign-in", info.system)
        } else {
            format!("Search, read and plan from {}'s tickets", info.system)
        };
        let response = crate::controls::primary(ui, &label, touch, &p).on_hover_text(hover);
        crate::topbar::record_item(ui, "sign-in", &response);
        if response.clicked() {
            client.sign_in_paste.clear();
            client.send(Command::SignInTickets);
        }
        let how = if view.opens_browser {
            "Opens your browser. ATOMICS keeps the sign-in in the OS keychain; the agents never see it."
        } else {
            "Gives you a link to open, then asks where the browser ended up."
        };
        ui.label(RichText::new(how).color(p.muted).size(11.0));
    };
    match touch {
        Some(_) => ui.vertical(row),
        None => ui.horizontal_wrapped(row),
    };
    ui.label(RichText::new(format!("system: {}  ·  {}", info.system, info.server)).size(11.0).color(p.muted));
    true
}

fn signing_in(ui: &mut egui::Ui, view: &SignInView, can: bool, client: &mut Client) {
    let info = &view.info;
    let p = client.palette.clone();
    let touch = client.touch;
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
    let manual = !view.opens_browser || info.browser_failed || client.sign_in_manual;
    ui.horizontal_wrapped(|ui| {
        let waiting = if manual { "Waiting for the sign-in…" } else { "Waiting for the browser…" };
        ui.label(RichText::new(waiting).strong());
        if can && touch::button(ui, touch, "Cancel").clicked() {
            client.send(Command::CancelSignIn);
        }
        if let Some(left) = view.seconds_left {
            ui.label(RichText::new(format!("({}:{:02})", left / 60, left % 60)).color(p.muted));
        }
    });
    if !manual {
        if can && touch::button(ui, touch, "No browser here?").clicked() {
            client.sign_in_manual = true;
        }
        return;
    }
    if info.browser_failed {
        ui.label(RichText::new("The browser could not be opened.").color(p.warning));
    }
    ui.label("Open this link on any machine, signed in as you:");
    match &info.url {
        Some(url) => {
            ui.horizontal_wrapped(|ui| {
                ui.add(egui::Label::new(RichText::new(url).monospace().size(11.0)).truncate());
                if ui
                    .add_enabled_ui(crate::markdown::web_url(url).is_some(), |ui| touch::button(ui, touch, "Open link"))
                    .inner
                    .clicked()
                {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                }
                if touch::button(ui, touch, "Copy link").clicked() {
                    ui.ctx().copy_text(url.clone());
                }
            });
        }
        None => {
            ui.label(RichText::new("Preparing the link…").color(p.muted));
        }
    }
    if !can {
        return;
    }
    ui.label("Then paste the address the browser ends on (it starts with http://127.0.0.1):");
    let mut use_it = false;
    ui.horizontal(|ui| {
        let width = (ui.available_width() - 80.0).max(120.0);
        let edit = crate::controls::single_line(&mut client.sign_in_paste)
            .hint_text("http://127.0.0.1:…/callback?code=…&state=…")
            .desired_width(width);
        let response = ui.add(edit);
        let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        use_it = touch::button(ui, touch, "Use it").clicked() || enter;
    });
    if use_it && !client.sign_in_paste.trim().is_empty() {
        let text = std::mem::take(&mut client.sign_in_paste);
        client.send(Command::PasteSignIn(text));
    }
    if let Some(why) = &info.rejected {
        ui.label(RichText::new(why).color(p.warning));
    }
}
