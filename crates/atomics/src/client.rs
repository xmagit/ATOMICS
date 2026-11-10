//! `atomics --connect <address>` (and `[client] connect`): the native window as a client of an
//! `atomics host` over `atomics.v1` (phase 3 in docs/daemon.md). The host owns agents, terminals, plans,
//! approvals, worktrees and git; this window owns only what a client does (selection, tabs, layout,
//! drafts). Closing it leaves every agent running on the host, and opening it again shows them as they
//! are.
//!
//! It is the browser client's own path, run natively: atomics-web's `Driver` (connection, reconnect and
//! backoff), `RemoteBackend` (the board, diffs, terminals, leases and mutations) and views (the desktop's
//! frame drawn by atomics-ui, the phone layout on a narrow window), with this module carrying out the
//! driver's outputs. Instead of the browser's broker and Noise channel, it dials the host's server
//! directly ([`link`]), with the host's bearer token: the local host's from its `protocol.json`, or a
//! paired one's ([`target`]). Instead of the page's input overlay, [`input`] routes the window's keys and
//! pointer to the terminal that has the keyboard.

mod credentials;
mod frame_stats;
pub mod input;
pub mod link;
pub mod target;
pub mod vm;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, Instant};

use atomics_ui::Client;
use atomics_web::driver::{Driver, Output, Status, Stop};
use atomics_web::views::{self, Place, View};
use egui::{Align, Layout, RichText};

use self::link::{Endpoint, Event, Link};
use self::target::Target;

/// What a launch asks of the window beyond showing the host: `--open AGENT:FOLDER` (started on the host
/// once its board is in, as the + New picker starts them) and `--screenshot` (a PNG after a delay, then
/// the window closes and the host goes on).
#[derive(Debug, Clone, Default)]
pub struct Launch {
    pub open: Vec<(String, std::path::PathBuf)>,
    pub screenshot: Option<(std::path::PathBuf, f32)>,
}

/// Finds the host's endpoint for one dial, on the dialing thread (it may take a moment).
pub type Resolve = Box<dyn FnOnce() -> anyhow::Result<Endpoint> + Send>;

/// The window as a client of the host `target` names.
pub struct ClientApp {
    previews: crate::preview::client::Client,
    driver: Driver,
    client: Client,
    view: View,
    target: Target,
    keyboard: input::Keyboard,
    events: Receiver<Event>,
    events_tx: SyncSender<Event>,
    link: Option<(u64, Link)>,
    wake: Arc<dyn Fn() + Send + Sync>,
    started: Instant,
    /// The VM's host: what the window does before it can dial (`vm.rs`).
    vm: Option<vm::Gate>,
    credentials: Option<credentials::Credentials>,
    /// Why forgetting the VM's refused token failed, for the dialog that asks to pair again.
    pair_error: Option<String>,
    /// A start of the VM ended: dial again at once.
    vm_started: Arc<AtomicBool>,
    /// The user asked to run agents on this Mac instead; the window asks first.
    confirm_local: bool,
    /// Why switching to this Mac failed.
    switch_error: Option<String>,
    /// Agents to start once the board is in.
    open: Vec<(String, std::path::PathBuf)>,
    /// `--screenshot`: where, when (seconds after the start), and whether it was taken.
    screenshot: Option<(std::path::PathBuf, f64, Arc<AtomicBool>)>,
    shot_requested: bool,
    /// `ATOMICS_FRAME_STATS`: frame rate and frame times written to a file, when set.
    frame_stats: Option<frame_stats::FrameStats>,
    /// Tests reach a host of their own making.
    #[cfg(test)]
    fixed: Option<Arc<dyn Fn() -> anyhow::Result<Endpoint> + Send + Sync>>,
}

/// Frames the link may read ahead of the window. A window that stops drawing makes the link stop
/// reading, and the host then treats it as the slow client it is (a fresh snapshot, or a drop), rather
/// than this process holding an unbounded backlog.
const READ_AHEAD: usize = 512;

/// A random prefix for this window's operation IDs, so a retried operation is recognised.
fn op_prefix() -> String {
    format!("desk-{}", &uuid::Uuid::new_v4().simple().to_string()[..16])
}

impl ClientApp {
    pub fn new(ctx: &egui::Context, target: Target, icon: Option<egui::TextureHandle>, launch: Launch) -> Self {
        let (events_tx, events) = mpsc::sync_channel(READ_AHEAD);
        let repaint = ctx.clone();
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || repaint.request_repaint());
        let mut client = Client::default();
        let mut view = View::default();
        // The configured theme, as the desktop would draw it; the window's state stays its own.
        let config = atomics_core::config::Config::load(&atomics_core::paths::config_file()).unwrap_or_default();
        let resolved = atomics_core::theme::load(
            &config.theme,
            &atomics_core::theme::themes_dir(&atomics_core::paths::config_file()),
        );
        view.looks = views::Looks::new(&resolved.tokens);
        client.palette = view.looks.desktop.clone();
        client.palette.apply(ctx, &views::BundledFonts);
        client.icon = icon;
        // A host with no window of its own (`atomics host`, the VM's) takes its terminals' sizes from these panes.
        view.fit_terminals = true;
        let vm = (target == Target::Vm).then(vm::Gate::new);
        view.place = Some(match &vm {
            Some(gate) => Place {
                text: format!("Agents in VM {}", gate.instance().unwrap_or("atomics")),
                hover: "This window shows the ATOMICS host in this Mac's VM: agents, the chief, hooks, git and \
                        builds run there. Closing the window leaves them running."
                    .into(),
                action: Some("Use this Mac".into()),
            },
            None => Place {
                text: "Agents on the host".into(),
                hover: format!(
                    "This window shows an ATOMICS host ({}): its agents run there, and closing the window leaves them \
                     running.",
                    match &target {
                        Target::Address { addr } => format!("ws://{addr}"),
                        _ => "atomics host on this machine".into(),
                    }
                ),
                // That host owns this machine's ATOMICS home, so this window cannot become the host too.
                action: None,
            },
        });
        let driver = Driver::direct(&op_prefix());
        let credentials = vm.as_ref().map(|gate| {
            driver.remote.enable_native_credentials(crate::secret_copy::unavailable());
            let mut credentials = credentials::Credentials::new(gate.bundle(), wake.clone());
            credentials.refresh();
            credentials
        });
        Self {
            previews: crate::preview::client::Client::new(wake.clone()),
            open: launch.open,
            screenshot: launch.screenshot.map(|(path, delay)| (path, f64::from(delay), Arc::default())),
            shot_requested: false,
            frame_stats: frame_stats::FrameStats::from_env(),
            vm,
            credentials,
            vm_started: Arc::default(),
            confirm_local: false,
            switch_error: None,
            driver,
            client,
            view,
            target,
            keyboard: input::Keyboard::default(),
            events,
            events_tx,
            link: None,
            wake,
            started: Instant::now(),
            pair_error: None,
            #[cfg(test)]
            fixed: None,
        }
    }

    /// A client of whatever `resolve` finds, for tests.
    #[cfg(test)]
    pub(crate) fn fixed(
        ctx: &egui::Context,
        resolve: impl Fn() -> anyhow::Result<Endpoint> + Send + Sync + 'static,
    ) -> Self {
        let mut app = Self::new(ctx, Target::Local, None, Launch::default());
        app.fixed = Some(Arc::new(resolve));
        // The first dial went out before the override: dial again with it.
        app.link = None;
        app.driver = Driver::direct(&op_prefix());
        app
    }

    /// A client of the VM's host behind `gate` that dials what `resolve` finds, for tests.
    #[cfg(test)]
    pub(crate) fn fixed_vm(
        ctx: &egui::Context,
        gate: vm::Gate,
        resolve: impl Fn() -> anyhow::Result<Endpoint> + Send + Sync + 'static,
    ) -> Self {
        let mut app = Self::fixed(ctx, resolve);
        app.vm = Some(gate);
        app
    }

    /// Why the last dial did not reach the host, as the window shows it.
    #[cfg(test)]
    pub(crate) fn unreachable(&self) -> Option<&str> {
        self.view.unreachable.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn driver(&self) -> &Driver {
        &self.driver
    }

    #[cfg(test)]
    pub(crate) fn driver_mut(&mut self) -> &mut Driver {
        &mut self.driver
    }

    /// The client's own work for one turn, with or without a frame: the clock, what the link reported,
    /// and what the driver asked for since.
    pub(crate) fn pump(&mut self) {
        if self.vm_started.swap(false, Ordering::AcqRel) {
            self.driver.retry_now();
        }
        let now = self.now_ms();
        self.driver.tick(now);
        self.take_events();
        self.pump_credentials();
        self.start_agents();
        self.pump_previews();
        self.driver.flush();
        self.run_outputs();
        self.pump_credentials();
        self.view.now_ms = now as f64;
    }

    /// Previews opened and closed as the board says. Their data moves on the relay's own thread, straight
    /// to and from the link, whether or not the window draws.
    fn pump_previews(&mut self) {
        let link = self.link.as_ref().map(|(socket, link)| (*socket, link.frames()));
        self.previews.pump(&self.driver.remote, link);
    }

    fn pump_credentials(&mut self) {
        if let Some(credentials) = &mut self.credentials {
            credentials.poll(&self.driver.remote);
        }
    }

    /// `--open`: once the host's board is in, each agent is started there, as the + New picker does.
    fn start_agents(&mut self) {
        let remote = &self.driver.remote;
        if self.open.is_empty() || !remote.live() || remote.with_mirror(|m| m.epoch.is_none()) {
            return;
        }
        for (agent, folder) in std::mem::take(&mut self.open) {
            atomics_ui::Backend::send(remote, atomics_ui::Command::Start { agent, folder: Some(folder) });
        }
        self.driver.flush();
    }

    /// `--screenshot`: the window as it is after the delay, then it closes.
    fn screenshot(&mut self, ctx: &egui::Context) {
        let Some((path, at, done)) = &self.screenshot else { return };
        if done.load(Ordering::Acquire) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if !self.shot_requested && self.started.elapsed().as_secs_f64() >= *at {
            self.shot_requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaintWhileHidden);
            let (path, done, repaint) = (path.clone(), done.clone(), ctx.clone());
            ctx.request_screenshot(move |image| {
                if let Err(error) = save_png(&path, &image) {
                    eprintln!("atomics: the screenshot failed: {error:#}");
                }
                done.store(true, Ordering::Release);
                repaint.request_repaint();
            });
        }
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    /// One frame of the window, after the terminal took its share of the input.
    pub(crate) fn draw(&mut self, ui: &mut egui::Ui) {
        if let Some(gate) = &self.vm {
            if !ui.ctx().input(|input| input.viewport().minimized.unwrap_or(false)) {
                gate.poll(self.wake.clone());
                ui.ctx().request_repaint_after(Duration::from_secs(5));
            }
            let (gate, wake) = (gate.clone(), self.wake.clone());
            let link = self.driver.status().clone();
            self.client.machine_controls = Some(Arc::new(move |ui| gate.controls(ui, &link, wake.clone())));
        }
        if !self.vm_screen(ui) {
            if let Some(gate) = &self.vm {
                let view = gate.view_for_host(self.driver.status());
                if matches!(view.state, vm::State::HostDisconnected | vm::State::Stopping) {
                    egui::Panel::top("vm-host-state").show(ui, |ui| {
                        ui.label(view.state.headline(&view.instance));
                    });
                }
            }
            views::draw(ui, &mut self.driver, &mut self.client, &mut self.view);
        } else {
            atomics_ui::machine::window(ui.ctx(), &self.driver.remote, &mut self.client);
        }
        self.pump_credentials();
        self.pump_previews();
        self.driver.flush();
        self.run_outputs();
        self.keyboard.follow(&mut self.view.term.overlay, &mut self.view.term.inputs);
        self.confirm_local |= std::mem::take(&mut self.view.place_clicked);
        self.pair_dialog(ui.ctx());
        self.switch_dialog(ui.ctx());
    }

    /// The VM's host refused this Mac's token, and nothing dials until the user says what to do: pair
    /// again, or run agents on this Mac. Never on its own: a new token is a new grant of the host.
    pub(crate) fn asks_to_pair_again(&self) -> bool {
        self.vm.is_some() && *self.driver.status() == Status::Stopped(Stop::Unauthorised)
    }

    /// "Pair again": forget the refused token and dial, which pairs anew over Lima's ssh.
    pub(crate) fn pair_again(&mut self) {
        let Some(gate) = &self.vm else { return };
        match gate.forget_token() {
            Ok(()) => {
                self.pair_error = None;
                // The refusal is answered: what the next dial finds is the news.
                self.view.unreachable = None;
                self.driver.retry_now();
            }
            Err(error) => self.pair_error = Some(format!("The keychain item could not be removed: {error:#}")),
        }
    }

    fn pair_dialog(&mut self, ctx: &egui::Context) {
        if !self.asks_to_pair_again() || self.confirm_local {
            return;
        }
        let instance = self.vm.as_ref().and_then(vm::Gate::instance).unwrap_or("atomics").to_string();
        let (mut pair, mut local) = (false, false);
        egui::Window::new("Pair with the VM's host again?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(format!(
                    "The ATOMICS host in the VM {instance} did not accept this Mac's token: it was unpaired there, \
                     or the VM's ATOMICS home was reset."
                ));
                ui.label(
                    "Pairing again asks that host for a new token over Lima's ssh and keeps it in this Mac's \
                     keychain in place of the old one.",
                );
                if let Some(error) = &self.pair_error {
                    ui.label(RichText::new(error).color(self.client.palette.warning));
                }
                ui.horizontal(|ui| {
                    pair = ui.button("Pair again").clicked();
                    local = ui.button("Use this Mac instead").clicked();
                });
            });
        if pair {
            self.pair_again();
        }
        self.confirm_local |= local;
    }

    /// Show stopped/starting VMs directly, retaining a mirrored board during link recovery.
    fn vm_screen(&mut self, ui: &mut egui::Ui) -> bool {
        let Some(gate) = &self.vm else { return false };
        let view = gate.view_for_host(self.driver.status());
        let recovering = matches!(self.driver.status(), Status::Connecting | Status::Reconnecting { .. });
        let has_board = self.driver.remote.with_mirror(|mirror| mirror.epoch.is_some());
        let unavailable = matches!(
            view.status,
            crate::vm::Status::Stopped | crate::vm::Status::NotSetUp | crate::vm::Status::Starting
        );
        if matches!(self.driver.status(), Status::Live)
            || (recovering && has_board && !unavailable && view.state != vm::State::Stopping)
        {
            return false;
        }
        let p = self.client.palette.clone();
        let mut start = false;
        egui::CentralPanel::default().frame(egui::Frame::new().fill(p.background)).show(ui, |ui| {
            ui.with_layout(Layout::top_down(Align::Center), |ui| {
                ui.add_space((ui.available_height() / 4.0).min(160.0));
                ui.label(RichText::new("ATOMICS").strong().size(18.0).color(p.accent).monospace());
                ui.add_space(12.0);
                let headline = view.state.headline(&view.instance);
                ui.label(RichText::new(headline).size(16.0));
                ui.add_space(6.0);
                ui.label(
                    RichText::new("Agents, the chief, hooks and git run in the VM; this window shows them.")
                        .color(p.muted),
                );
                ui.add_space(14.0);
                match &view.state {
                    vm::State::Vm(crate::vm::Status::NotSetUp) => {
                        ui.label("Run atomics vm setup to download and set up this version's Linux VM.");
                    }
                    vm::State::Vm(crate::vm::Status::Starting) => {
                        ui.spinner();
                    }
                    vm::State::HostDisconnected => {
                        ui.label("Waiting to reconnect to the VM's host…");
                    }
                    vm::State::Unauthorised => {
                        ui.label("Pair again to reconnect to the VM's host.");
                    }
                    vm::State::Stopping => {
                        ui.spinner();
                    }
                    vm::State::Vm(crate::vm::Status::Running) => {}
                    _ => start = ui.button(RichText::new(format!("Start {}", view.instance)).size(14.0)).clicked(),
                }
                if let Some(note) = view.note.as_ref().filter(|_| !view.starting) {
                    ui.add_space(10.0);
                    ui.label(RichText::new(note).color(p.warning).small());
                }
                ui.add_space(18.0);
                if ui.button("Machine").clicked() {
                    self.client.machine_open = true;
                }
                if ui.small_button("Use this Mac instead").clicked() {
                    self.confirm_local = true;
                }
            });
        });
        if start {
            let started = self.vm_started.clone();
            let wake = self.wake.clone();
            gate.start(Arc::new(move || {
                started.store(true, Ordering::Release);
                wake();
            }));
        }
        true
    }

    /// "Use this Mac": asked once, then the window opens again as the host of agents on this Mac. The VM's
    /// agents keep running there.
    fn switch_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_local {
            return;
        }
        let mut close = false;
        egui::Window::new("Run agents in this window?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(
                    "ATOMICS opens again as the host itself: new agents start on this Mac, as without a VM. \
                     The host this window shows keeps its agents running; open it again with atomics --connect.",
                );
                ui.label(
                    RichText::new("[client] connect is removed from your config (a backup is kept beside it).").small(),
                );
                if let Some(error) = &self.switch_error {
                    ui.label(RichText::new(error).color(self.client.palette.warning));
                }
                ui.horizontal(|ui| {
                    if ui.button("Use this window").clicked() {
                        match switch_to_local() {
                            Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                            Err(error) => self.switch_error = Some(format!("{error:#}")),
                        }
                    }
                    close = ui.button("Cancel").clicked();
                });
            });
        if close {
            self.confirm_local = false;
            self.switch_error = None;
        }
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// How the next dial finds the host.
    fn resolver(&self) -> Resolve {
        #[cfg(test)]
        if let Some(fixed) = self.fixed.clone() {
            return Box::new(move || fixed());
        }
        match (self.target.clone(), &self.vm) {
            (_, Some(gate)) => gate.resolver(),
            (Target::Local, _) => Box::new(target::local),
            (Target::Address { addr }, _) => Box::new(move || target::address(addr)),
            (Target::Vm, None) => Box::new(|| anyhow::bail!("the VM's host is reached through `atomics --connect vm`")),
        }
    }

    fn run_outputs(&mut self) {
        loop {
            let outputs = self.driver.take_outputs();
            if outputs.is_empty() {
                return;
            }
            for output in outputs {
                match output {
                    Output::DialDirect { socket } => {
                        let link = Link::open(
                            socket,
                            self.resolver(),
                            self.events_tx.clone(),
                            self.wake.clone(),
                            self.previews.tap(),
                        );
                        self.link = Some((socket, link));
                    }
                    Output::Send { socket, message } => {
                        if let Some((_, link)) = self.link.as_ref().filter(|(id, _)| *id == socket) {
                            link.send(String::from_utf8_lossy(&message).into_owned());
                        }
                    }
                    Output::Close { socket, .. } => {
                        if let Some((_, link)) = self.link.take_if(|(id, _)| *id == socket) {
                            link.close();
                            self.previews.reset();
                        }
                    }
                    // A direct client has no hosting page, no stored device and no passkey, so the driver never
                    // asks it to keep, forget or unlock a sealed copy.
                    Output::Dial { socket, .. } => self.driver.socket_closed(socket, 1006),
                    Output::Post(_)
                    | Output::LoadDevice(_)
                    | Output::SaveDevice(_)
                    | Output::ForgetDevice(_)
                    | Output::Paired { .. }
                    | Output::Keep(_)
                    | Output::Forget(_)
                    | Output::Passkey(_) => {}
                }
            }
        }
    }

    /// What the link reported since the last frame, in order.
    fn take_events(&mut self) {
        for event in self.events.try_iter().collect::<Vec<_>>() {
            match event {
                Event::Open { socket, protocol } => {
                    self.view.unreachable = None;
                    self.driver.socket_open(socket, &protocol);
                    if self.link.as_ref().is_some_and(|(id, _)| *id == socket)
                        && let Some(credentials) = &mut self.credentials
                    {
                        credentials.refresh();
                    }
                }
                Event::Data { socket, bytes } => self.driver.socket_message(socket, &bytes),
                Event::Closed { socket, code, unreachable } => {
                    if self.link.as_ref().is_some_and(|(id, _)| *id == socket) {
                        self.link = None;
                        self.previews.reset();
                    }
                    // Why the host could not be reached: on screen while the window waits for it, and in
                    // the log each time it changes.
                    if let Some(why) = unreachable.filter(|why| self.view.unreachable.as_ref() != Some(why)) {
                        eprintln!("atomics: the host cannot be reached: {why}");
                        self.view.unreachable = Some(why);
                    }
                    // A refused token stops the driver (`Stop::Unauthorised`); for the VM's host the window
                    // then asks before it pairs again (`pair_dialog`).
                    self.driver.socket_closed(socket, code);
                }
            }
        }
    }
}

impl eframe::App for ClientApp {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        let text_box = ctx.memory(|m| m.focused().is_some());
        let overlay = self.view.term.overlay.clone();
        let now = self.now_ms() as f64;
        let mut copy = |text: String| ctx.copy_text(text);
        self.keyboard.route(raw, &overlay, text_box, now, &mut self.view.term.inputs, &mut copy);
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(stats) = &mut self.frame_stats {
            stats.begin(Instant::now());
        }
        self.pump();
        self.screenshot(ctx);
        if let Some(wait) = self.driver.wake_in() {
            ctx.request_repaint_after(Duration::from_millis(wait.max(16)));
        }
        if let Some(due) = self.view.term.due_ms() {
            ctx.request_repaint_after(Duration::from_millis((due - self.view.now_ms).max(16.0) as u64));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
        if let Some(stats) = &mut self.frame_stats {
            stats.end(Instant::now());
        }
    }
}

fn save_png(path: &std::path::Path, image: &egui::ColorImage) -> anyhow::Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut encoder = png::Encoder::new(file, image.size[0] as u32, image.size[1] as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let bytes: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
    encoder.write_header()?.write_image_data(&bytes)?;
    Ok(())
}

/// Take `[client] connect` out of the config file that sets it, and open ATOMICS again as the host, with
/// this launch's arguments but `--connect`.
fn switch_to_local() -> anyhow::Result<()> {
    forget_connect_setting()?;
    relaunch_without_connect()
}

/// A `connect` value ATOMICS cannot open, said where it is seen. Opened from the Dock or a launcher there is no
/// terminal for stderr, so a value from the config is said in a dialog too, rather than quitting silently.
/// `--connect` on a command line is said on stderr alone.
pub(crate) fn report_bad_connect(value: &str, from_command_line: bool, error: &anyhow::Error) {
    use std::io::IsTerminal;
    let file = (!from_command_line).then(connect_setting_file).flatten();
    let message = bad_connect_message(value, from_command_line, file.as_deref(), error);
    eprintln!("atomics: {message}");
    if !from_command_line && !std::io::stderr().is_terminal() {
        let _ = rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("ATOMICS cannot open")
            .set_description(&message)
            .set_buttons(rfd::MessageButtons::Ok)
            .show();
    }
}

fn bad_connect_message(
    value: &str,
    from_command_line: bool,
    file: Option<&std::path::Path>,
    error: &anyhow::Error,
) -> String {
    if from_command_line {
        return format!("--connect {value:?}: {error:#}.");
    }
    let place = file.map(|file| format!(" in {}", file.display())).unwrap_or_default();
    format!(
        "[client] connect = {value:?}{place}: {error:#}. Correct it, or remove it to run agents on this Mac; \
         `atomics --connect \"\"` does that for one run."
    )
}

/// The config file whose `[client] connect` is in force: the host file over the main one.
fn connect_setting_file() -> Option<std::path::PathBuf> {
    use atomics_core::selfconfig::{self, Layer, Source};
    let [main, host] = atomics_core::config::Config::source_paths(&atomics_core::paths::config_file());
    let key = ["client".to_string(), "connect".to_string()];
    [(Source::Host, host), (Source::Main, main)]
        .into_iter()
        .find(|(source, file)| selfconfig::defines(&Layer::read(*source, file).table, &key))
        .map(|(_, file)| file)
}

/// Take `[client] connect` out of every config file that sets it (the host file, the main file), each with
/// a backup beside it; the files it changed.
pub(crate) fn forget_connect_setting() -> anyhow::Result<Vec<std::path::PathBuf>> {
    use atomics_core::selfconfig::{self, Layer, Scope, Source};
    let [main, host] = atomics_core::config::Config::source_paths(&atomics_core::paths::config_file());
    let layers = [Layer::read(Source::Main, &main), Layer::read(Source::Host, &host)];
    let key = ["client".to_string(), "connect".to_string()];
    let mut changed = Vec::new();
    for (scope, file, layer) in [(Scope::Host, &host, &layers[1]), (Scope::Main, &main, &layers[0])] {
        if selfconfig::defines(&layer.table, &key) {
            let proposal = selfconfig::propose(scope, &layers, "", &["client.connect".to_string()])
                .map_err(|why| anyhow::anyhow!("{why}"))?;
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            selfconfig::write(file, proposal.before.as_deref(), &proposal.after, stamp)?;
            changed.push(file.clone());
        }
    }
    Ok(changed)
}

/// ATOMICS again, with this launch's arguments but `--connect`, once this process has ended.
fn relaunch_without_connect() -> anyhow::Result<()> {
    let mut args = Vec::new();
    let mut skip = false;
    for arg in std::env::args_os().skip(1) {
        if std::mem::take(&mut skip) {
            continue;
        }
        let text = arg.to_string_lossy();
        if text == "--connect" {
            skip = true;
        } else if !text.starts_with("--connect=") {
            args.push(arg);
        }
    }
    let target = crate::restart::Target::with_args(&args)?;
    target.spawn(std::process::id(), &atomics_core::paths::atomics_home())?;
    Ok(())
}

/// Run the window as a client of `target` until it is closed. The host and its agents go on.
pub fn run(target: Target, launch: Launch, native: eframe::NativeOptions) -> eframe::Result {
    let icon = native.viewport.icon.clone();
    eframe::run_native(
        "ATOMICS",
        native,
        Box::new(move |cc| {
            let texture = icon.map(|icon| {
                let image =
                    egui::ColorImage::from_rgba_unmultiplied([icon.width as usize, icon.height as usize], &icon.rgba);
                cc.egui_ctx.load_texture("atomics-icon", image, egui::TextureOptions::LINEAR)
            });
            Ok(Box::new(ClientApp::new(&cc.egui_ctx, target, texture, launch)))
        }),
    )
}

#[cfg(test)]
mod tests;
