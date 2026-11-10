//! The window's and the host's side of updating: the check on start and every 6 hours, the Update button's job, and
//! the wait to restart into what it installed. What is installed, and how, is [`crate::update`]. The restart is
//! always the safe one and never forced: it waits for [`State::update_blockers`], then for [`COUNTDOWN`].

use super::*;
use crate::update::{CHECK_EVERY, Found, Mode};
use atomics_core::config::UpdateConfig;
use atomics_core::projection::{UpdateInfo, UpdatePhase};
use std::sync::mpsc::TryRecvError;

/// How long the window stays once nothing known is in the way, so that someone can stop it for what the host cannot
/// know of: a half-typed prompt, or a program a shell left unsaved in the background.
const COUNTDOWN: Duration = Duration::from_secs(10);

enum JobEvent {
    Step(String),
    Done(Result<(), String>),
}

pub(super) struct Updates {
    /// `[update]` as what follows read it.
    config: UpdateConfig,
    current: String,
    found: Option<Found>,
    /// Why `found` cannot be installed here.
    unavailable: Option<String>,
    check_error: Option<String>,
    phase: UpdatePhase,
    /// The version installed or being installed.
    version: Option<String>,
    next_check: Option<Instant>,
    /// When the window closes, once nothing known is in the way.
    restart_at: Option<Instant>,
    check: Option<Receiver<Result<Option<Found>, String>>>,
    job: Option<Receiver<JobEvent>>,
    /// In a test, where updates come from: nothing there reaches the network unless it says so.
    #[cfg(test)]
    test_mode: Option<Mode>,
}

/// When a host with this config first looks.
fn first_check(config: &UpdateConfig) -> Option<Instant> {
    (config.check && !cfg!(test)).then(Instant::now)
}

impl Updates {
    pub(super) fn new(config: &Config) -> Updates {
        Updates {
            config: config.update.clone(),
            current: Mode::of(config).current(),
            found: None,
            unavailable: None,
            check_error: None,
            phase: UpdatePhase::Idle,
            version: None,
            next_check: first_check(&config.update),
            restart_at: None,
            check: None,
            job: None,
            #[cfg(test)]
            test_mode: None,
        }
    }

    /// An update's own job is running, which holds a restart back.
    pub(super) fn installing(&self) -> bool {
        self.job.is_some()
    }
}

/// The same file, by whichever name.
fn same_program(first: &Path, second: &Path) -> bool {
    let resolved = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    resolved(first) == resolved(second)
}

impl State {
    fn update_mode(&self) -> Mode {
        #[cfg(test)]
        if let Some(mode) = &self.update.test_mode {
            return mode.clone();
        }
        Mode::of(&self.config)
    }

    pub(crate) fn update_info(&self) -> UpdateInfo {
        let update = &self.update;
        UpdateInfo {
            current: update.current.clone(),
            latest: update.found.as_ref().map(|found| found.version.clone()),
            detail: update.found.as_ref().and_then(|found| found.detail.clone()),
            notes: update.found.as_ref().and_then(|found| found.notes.clone()),
            check_error: update.check_error.clone(),
            unavailable: update.unavailable.clone(),
            phase: update.phase.clone(),
        }
    }

    pub(super) fn poll_update(&mut self) -> bool {
        let mut changed = self.read_update_config();
        if let Some(check) = &self.update.check {
            match check.try_recv() {
                Ok(result) => {
                    self.finish_check(result);
                    changed = true;
                }
                Err(TryRecvError::Disconnected) => self.update.check = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        self.start_check_when_due();
        changed |= self.read_update_job();
        if matches!(self.update.phase, UpdatePhase::WaitingForAgents { .. } | UpdatePhase::RestartsIn { .. }) {
            let workspace = self.workspace.clone();
            changed |= self.restart_when_clear(move || crate::restart::Target::current(workspace.as_deref()));
        }
        changed
    }

    /// `[update]` changed: what was found under the old one no longer holds. With checks off the host knows of no
    /// new version, and shows none.
    fn read_update_config(&mut self) -> bool {
        if self.update.config == self.config.update {
            return false;
        }
        let mode = self.update_mode();
        let update = &mut self.update;
        update.config = self.config.update.clone();
        update.current = mode.current();
        update.found = None;
        update.unavailable = None;
        update.check_error = None;
        update.check = None;
        update.next_check = first_check(&update.config);
        true
    }

    fn start_check_when_due(&mut self) {
        let update = &self.update;
        let idle = matches!(update.phase, UpdatePhase::Idle | UpdatePhase::Failed { .. });
        let due = update.next_check.is_some_and(|at| Instant::now() >= at);
        if !due || !idle || !update.config.check || update.check.is_some() || update.job.is_some() {
            return;
        }
        // One look, and the next is CHECK_EVERY after its answer, whatever the answer was.
        self.update.next_check = None;
        let mode = self.update_mode();
        let (tx, rx) = self.wake.channel();
        self.update.check = Some(rx);
        let spawned = std::thread::Builder::new().name("atomics-update-check".into()).spawn(move || {
            let _ = tx.send(mode.check().map_err(|error| format!("{error:#}")));
        });
        if spawned.is_err() {
            self.update.check = None;
            self.update.next_check = Some(Instant::now() + CHECK_EVERY);
        }
    }

    /// A failure is silent: the top bar and the notice line never hear of it, only Settings does. What an earlier
    /// check found stays.
    fn finish_check(&mut self, result: Result<Option<Found>, String>) {
        let mode = self.update_mode();
        let update = &mut self.update;
        update.check = None;
        update.next_check = Some(Instant::now() + CHECK_EVERY);
        match result {
            Ok(found) => {
                update.unavailable = found.as_ref().and_then(|found| mode.unavailable(found));
                // Nothing is left to retry.
                if found.is_none() && matches!(update.phase, UpdatePhase::Failed { .. }) {
                    update.phase = UpdatePhase::Idle;
                }
                update.found = found;
                update.check_error = None;
            }
            Err(error) => update.check_error = Some(error),
        }
    }

    /// The Update button: install what the last check found, or, once it is installed, wait to restart into it.
    pub(super) fn start_update(&mut self) {
        match &self.update.phase {
            UpdatePhase::Installed { .. } => {
                self.update.phase = UpdatePhase::WaitingForAgents { blockers: self.update_blockers() };
                return;
            }
            UpdatePhase::Idle | UpdatePhase::Failed { .. } => {}
            UpdatePhase::Working { .. }
            | UpdatePhase::WaitingForAgents { .. }
            | UpdatePhase::RestartsIn { .. }
            | UpdatePhase::Unknown => return,
        }
        let Some(found) = self.update.found.clone() else { return };
        if let Some(why) = &self.update.unavailable {
            let why = why.clone();
            self.toast(why);
            return;
        }
        let mode = self.update_mode();
        let home = paths::atomics_home();
        let (tx, rx) = self.wake.channel();
        self.update.version = Some(found.version.clone());
        self.update.phase = UpdatePhase::Working { step: format!("Starting the update to {}", found.version) };
        self.update.job = Some(rx);
        let spawned = std::thread::Builder::new().name("atomics-update".into()).spawn(move || {
            let result = crate::update::lock(&home)
                .and_then(|_lock| mode.install(&found, &|step| drop(tx.send(JobEvent::Step(step)))));
            let _ = tx.send(JobEvent::Done(result.map_err(|error| format!("{error:#}"))));
        });
        if let Err(error) = spawned {
            self.update.job = None;
            self.update_failed(format!("Could not start the update: {error}. The installed version was not changed."));
        }
    }

    fn read_update_job(&mut self) -> bool {
        let mut changed = false;
        while let Some(event) = self.update.job.as_ref().map(Receiver::try_recv) {
            match event {
                Ok(JobEvent::Step(step)) => self.update.phase = UpdatePhase::Working { step },
                Ok(JobEvent::Done(result)) => {
                    self.update.job = None;
                    match result {
                        Ok(()) => {
                            self.update.phase = UpdatePhase::WaitingForAgents { blockers: self.update_blockers() };
                        }
                        Err(error) => self.update_failed(error),
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.update.job = None;
                    self.update_failed("The update stopped unexpectedly.".into());
                }
            }
            changed = true;
        }
        changed
    }

    /// Someone asked for this, so unlike a check's failure it says why where they are looking.
    fn update_failed(&mut self, error: String) {
        self.toast(format!("Update failed: {error}"));
        self.update.phase = UpdatePhase::Failed { error };
    }

    /// `atomics update` installed a version from a terminal.
    pub(super) fn update_installed(&mut self, version: String, program: Option<String>) -> ControlResponse {
        if self.update.installing() {
            return ControlResponse::error("this ATOMICS is installing an update itself");
        }
        // Restarting would only rerun the program it already is.
        if let (Some(program), Ok(running)) = (program, crate::setup::executable_path())
            && !same_program(Path::new(&program), &running)
        {
            return ControlResponse::error(format!(
                "ATOMICS is running from {}, which this did not update; start {program} to use the new version",
                running.display()
            ));
        }
        self.update.version = Some(version);
        self.update.restart_at = None;
        self.update.phase = UpdatePhase::WaitingForAgents { blockers: self.update_blockers() };
        ControlResponse::ok(json!({"waiting": true}))
    }

    /// Says what is in the way, and once nothing is, counts down and restarts. Anything that comes up during the
    /// countdown stops it, and the next one starts over. Nothing here can force it.
    fn restart_when_clear(&mut self, target: impl FnOnce() -> anyhow::Result<crate::restart::Target>) -> bool {
        let blockers = self.update_blockers();
        if !blockers.is_empty() {
            self.update.restart_at = None;
            return self.show_update_phase(UpdatePhase::WaitingForAgents { blockers });
        }
        let now = Instant::now();
        let at = *self.update.restart_at.get_or_insert(now + COUNTDOWN);
        if now < at {
            return self.show_update_phase(UpdatePhase::RestartsIn { seconds: (at - now).as_secs_f64().ceil() as u64 });
        }
        self.update.restart_at = None;
        let version = self.update.version.clone().unwrap_or_default();
        let response = self.restart_using(false, Some(format!("update to {version}")), target);
        if !response.ok {
            let error = response.error.unwrap_or_default();
            self.toast(format!("Could not restart into {version}: {error}"));
            self.update.phase = UpdatePhase::Installed { version };
        }
        true
    }

    fn show_update_phase(&mut self, phase: UpdatePhase) -> bool {
        let changed = self.update.phase != phase;
        self.update.phase = phase;
        changed
    }

    /// The Cancel button: the new version stays installed, and this host stays on the old one for now.
    pub(super) fn cancel_update_wait(&mut self) {
        if matches!(self.update.phase, UpdatePhase::WaitingForAgents { .. } | UpdatePhase::RestartsIn { .. }) {
            self.update.restart_at = None;
            let version = self.update.version.clone().unwrap_or_default();
            self.update.phase = UpdatePhase::Installed { version };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::key_tests::{two_agents, untouched};
    use super::super::tests::{session, with_app};
    use super::*;

    #[test]
    fn a_host_running_another_program_than_the_one_updated_says_so_and_stays_put() {
        with_app(
            "app::update::tests::a_host_running_another_program_than_the_one_updated_says_so_and_stays_put",
            |app, _| {
                let elsewhere = app.runtime.update_installed("9.0.0".into(), Some("/elsewhere/atomics".into()));
                let error = elsewhere.error.unwrap();
                assert!(error.contains("which this did not update") && error.contains("/elsewhere/atomics"), "{error}");
                assert!(matches!(app.runtime.update.phase, UpdatePhase::Idle), "nothing is armed");
                let running = crate::setup::executable_path().unwrap().display().to_string();
                assert!(app.runtime.update_installed("9.0.0".into(), Some(running)).ok);
                assert!(matches!(app.runtime.update.phase, UpdatePhase::WaitingForAgents { .. }));
                app.runtime.update.phase = UpdatePhase::Idle;
            },
        );
    }
    use crate::update::test_support::{Release, found, signed_feed_failing, signed_feed_with};

    /// One frame of the whole window, `width` wide, and what the top bar's items recorded.
    fn paint(
        app: &mut Desktop<'_>,
        ctx: &egui::Context,
        width: f32,
        events: Vec<egui::Event>,
    ) -> (Vec<(String, Rect)>, egui::FullOutput) {
        ctx.data_mut(|d| d.insert_temp(egui::Id::NULL.with("topbar-items"), Vec::<(String, Rect)>::new()));
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(width, 800.0))),
                events,
                ..Default::default()
            },
            |ui| app.paint(ui),
        );
        app.runtime.drain_actions();
        let mut items = ctx.data(|d| d.get_temp::<Vec<(String, Rect)>>(egui::Id::NULL.with("topbar-items")).unwrap());
        // The location line under the bar records its links too.
        items.retain(|(name, _)| !name.starts_with("location"));
        (items, output)
    }

    fn items(app: &mut Desktop<'_>, ctx: &egui::Context, width: f32) -> Vec<(String, Rect)> {
        let (items, output) = paint(app, ctx, width, vec![]);
        output.drop_without_applying_deltas();
        items
    }

    fn drawn(app: &mut Desktop<'_>, ctx: &egui::Context, width: f32) -> Vec<String> {
        let (_, output) = paint(app, ctx, width, vec![]);
        let text = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                _ => None,
            })
            .collect();
        output.drop_without_applying_deltas();
        text
    }

    fn click(app: &mut Desktop<'_>, ctx: &egui::Context, width: f32, name: &str) {
        let at = items(app, ctx, width)
            .iter()
            .find(|(item, _)| item == name)
            .unwrap_or_else(|| panic!("{name} is not in the bar"))
            .1
            .center();
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        for events in [vec![egui::Event::PointerMoved(at)], vec![button(true)], vec![button(false)], vec![]] {
            paint(app, ctx, width, events).1.drop_without_applying_deltas();
        }
    }

    fn select_updates(app: &mut Desktop<'_>, ctx: &egui::Context, width: f32) {
        let (_, output) = paint(app, ctx, width, vec![]);
        let at = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "Updates" => {
                    Some(text.visual_bounding_rect().center())
                }
                _ => None,
            })
            .expect("Updates is in the Settings section list");
        output.drop_without_applying_deltas();
        for pressed in [true, false] {
            paint(
                app,
                ctx,
                width,
                vec![
                    egui::Event::PointerMoved(at),
                    egui::Event::PointerButton {
                        pos: at,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            )
            .1
            .drop_without_applying_deltas();
        }
    }

    /// As a check that found a release leaves it.
    fn new_version(app: &mut Desktop<'_>) {
        app.runtime.update.found = Some(found("9.0.0"));
        app.runtime.update.unavailable = None;
    }

    #[test]
    fn the_item_survives_every_width_of_the_bar_and_folds_with_the_other_actions() {
        with_app(
            "app::update::tests::the_item_survives_every_width_of_the_bar_and_folds_with_the_other_actions",
            |app, ctx| {
                app.runtime.repos = vec![PathBuf::from("/project/ATOMICS")];
                app.runtime.sessions =
                    vec![session("agent", AgentKind::Generic, AgentStatus::Idle, Path::new("/project/ATOMICS"))];
                new_version(app);
                let phases: [(UpdatePhase, &str); 4] = [
                    (UpdatePhase::Idle, "Update"),
                    (UpdatePhase::Working { step: "Downloading 9.0.0".into() }, "Update"),
                    (UpdatePhase::WaitingForAgents { blockers: vec!["agent x is working".into()] }, "Cancel"),
                    (UpdatePhase::Installed { version: "9.0.0".into() }, "Restart to update"),
                ];
                for (phase, name) in phases {
                    app.runtime.update.phase = phase.clone();
                    for width in [480.0, 550.0, 700.0, 900.0, 1200.0, 1500.0, 1700.0, 2400.0, 5120.0] {
                        let case = format!("width={width}, {phase:?}");
                        let row = items(app, ctx, width);
                        let collapsed = row.iter().any(|(item, _)| item == "Actions");
                        let under_way =
                            matches!(phase, UpdatePhase::Working { .. } | UpdatePhase::WaitingForAgents { .. });
                        // An update under way stays beside the folded menu where it fits.
                        let beside = collapsed && under_way && row.iter().any(|(item, _)| item == name);
                        assert_eq!(
                            row.iter().filter(|(item, _)| item == name).count(),
                            usize::from(!collapsed || beside),
                            "{case}: in the bar, or folded into Actions: {row:?}"
                        );
                        if under_way && width >= 1500.0 {
                            assert!(!collapsed || beside, "{case}: there is room for it: {row:?}");
                        }
                        let bar = Rect::from_min_size(pos2(10.0, 0.0), vec2(width - 20.0, 34.0)).expand(0.5);
                        for (at, (item, rect)) in row.iter().enumerate() {
                            assert!(bar.contains_rect(*rect), "{case}: {item} outside the bar: {row:?}");
                            for (other, other_rect) in &row[at + 1..] {
                                assert!(!rect.shrink(0.1).intersects(*other_rect), "{case}: {item} over {other}");
                            }
                        }
                        let chief = row.iter().find(|(item, _)| item == "chief").unwrap().1;
                        assert!((chief.center().x - width / 2.0).abs() < 1.0, "{case}: the pill stays in the middle");
                        assert_eq!(dot(app, ctx, width), collapsed && !beside, "{case}: the dot on Actions");
                        if collapsed && !beside {
                            click(app, ctx, width, "Actions");
                            // A menu sizes before its first paint.
                            items(app, ctx, width);
                            let inside = items(app, ctx, width);
                            assert!(
                                inside.iter().any(|(item, _)| item == name),
                                "{case}: in the Actions menu: {inside:?}"
                            );
                            let escape = egui::Event::Key {
                                key: Key::Escape,
                                physical_key: None,
                                pressed: true,
                                repeat: false,
                                modifiers: egui::Modifiers::NONE,
                            };
                            paint(app, ctx, width, vec![escape]).1.drop_without_applying_deltas();
                        }
                    }
                }
            },
        );
    }

    /// Whether the Actions button wears the dot, whole, inside the button. The chief's pill has accent circles of its
    /// own, so only what is on the button counts.
    fn dot(app: &mut Desktop<'_>, ctx: &egui::Context, width: f32) -> bool {
        dot_color(app, ctx, width).is_some()
    }

    fn dot_color(app: &mut Desktop<'_>, ctx: &egui::Context, width: f32) -> Option<egui::Color32> {
        let (accent, warning) = (app.palette().accent, app.palette().warning);
        let (row, output) = paint(app, ctx, width, vec![]);
        let actions = row.iter().find(|(item, _)| item == "Actions").map(|(_, rect)| *rect);
        let found = actions.and_then(|actions| {
            output.shapes.iter().find_map(|shape| match &shape.shape {
                egui::Shape::Circle(circle)
                    if [accent, warning].contains(&circle.fill) && actions.contains(circle.center) =>
                {
                    assert!(actions.contains_rect(circle.visual_bounding_rect()), "{circle:?} outside {actions:?}");
                    Some(circle.fill)
                }
                _ => None,
            })
        });
        output.drop_without_applying_deltas();
        found
    }

    #[test]
    fn a_dot_on_the_folded_actions_says_an_update_is_in_there_and_moves_nothing() {
        with_app(
            "app::update::tests::a_dot_on_the_folded_actions_says_an_update_is_in_there_and_moves_nothing",
            |app, ctx| {
                app.runtime.repos = vec![PathBuf::from("/project/ATOMICS")];
                app.runtime.sessions =
                    vec![session("agent", AgentKind::Generic, AgentStatus::Idle, Path::new("/project/ATOMICS"))];
                let width = 700.0;
                let rect = |row: &[(String, Rect)], name: &str| {
                    row.iter().find(|(item, _)| item == name).unwrap_or_else(|| panic!("{name}: {row:?}")).1
                };
                // Twice: the first frame of a menu or a tab measures itself.
                items(app, ctx, width);
                let before = items(app, ctx, width);
                assert!(!dot(app, ctx, width), "nothing pending, no dot");

                // Update is in the folded menu: the dot shows, and every item stays where it was.
                new_version(app);
                items(app, ctx, width);
                let after = items(app, ctx, width);
                assert!(!after.iter().any(|(item, _)| item == "Update"), "folded: {after:?}");
                assert!(dot(app, ctx, width), "Update is in the menu");
                for name in ["Actions", "chief", "view:Board", "view:Diff"] {
                    assert_eq!(rect(&after, name), rect(&before, name), "{name} moved");
                }

                // As does a failed try, which is there to press again, and Restart to update.
                let failed = UpdatePhase::Failed { error: "It failed.".into() };
                let installed = UpdatePhase::Installed { version: "9.0.0".into() };
                for phase in [failed, installed] {
                    app.runtime.update.phase = phase.clone();
                    assert!(dot(app, ctx, width), "{phase:?}");
                }

                // With no room for it beside the menu, an update under way marks the menu in another colour: pressing
                // Update must change what the bar shows, and a restart nobody can see coming must not happen.
                let working = UpdatePhase::Working { step: "Downloading 9.0.0".into() };
                let waiting = UpdatePhase::WaitingForAgents { blockers: vec!["agent x is working".into()] };
                let narrow = 480.0;
                assert_eq!(dot_color(app, ctx, narrow), Some(app.palette().accent));
                for phase in [working, waiting] {
                    app.runtime.update.phase = phase.clone();
                    let row = items(app, ctx, narrow);
                    assert!(!row.iter().any(|(item, _)| item == "Update" || item == "Cancel"), "no room: {row:?}");
                    assert_eq!(dot_color(app, ctx, narrow), Some(app.palette().warning), "{phase:?}");
                }
                app.runtime.update.phase = UpdatePhase::Idle;
                app.runtime.update.unavailable = Some("This build has no release signing key.".into());
                assert!(!dot(app, ctx, width), "only a hint is in the menu");
                app.runtime.update.unavailable = None;
                app.runtime.update.found = None;
                assert!(!dot(app, ctx, width), "nothing new");

                // With room the button is in the bar, and there is no menu to mark.
                new_version(app);
                items(app, ctx, 5120.0);
                let wide = items(app, ctx, 5120.0);
                assert!(wide.iter().any(|(item, _)| item == "Update"), "{wide:?}");
                assert!(!dot(app, ctx, 5120.0));
            },
        );
    }

    #[test]
    fn nothing_shows_while_nothing_is_new_and_a_failed_check_shows_only_in_settings() {
        with_app(
            "app::update::tests::nothing_shows_while_nothing_is_new_and_a_failed_check_shows_only_in_settings",
            |app, ctx| {
                let toast = app.runtime.toast.clone();
                let width = 2400.0;
                let text = drawn(app, ctx, width);
                assert!(!text.iter().any(|t| t == "Update" || t.starts_with("Updates")), "{text:?}");
                app.client.ui.management.settings = true;
                paint(app, ctx, width, vec![]).1.drop_without_applying_deltas();
                let text = drawn(app, ctx, width);
                assert!(text.iter().any(|t| t == "Where agents run"), "Settings is open on Agents: {text:?}");
                assert!(!text.iter().any(|t| t.starts_with("This version:")), "details wait for selection: {text:?}");

                app.runtime.finish_check(Err("Could not reach GitHub: timed out".into()));
                assert_eq!(app.runtime.toast, toast, "the notice line never hears of it");
                let bar = items(app, ctx, width);
                assert!(!bar.iter().any(|(item, _)| item == "Update"), "{bar:?}");
                paint(app, ctx, width, vec![]).1.drop_without_applying_deltas();
                select_updates(app, ctx, width);
                let text = drawn(app, ctx, width);
                assert!(text.iter().any(|t| t.starts_with("Updates")), "{text:?}");
                assert!(
                    text.iter().any(|t| t == "The last check failed: Could not reach GitHub: timed out"),
                    "{text:?}"
                );
                assert!(
                    app.runtime.update.next_check.is_some_and(|at| at > Instant::now() + Duration::from_secs(5 * 3600)),
                    "no retry before the next turn"
                );
            },
        );
    }

    #[test]
    fn settings_says_the_versions_and_links_the_notes() {
        with_app("app::update::tests::settings_says_the_versions_and_links_the_notes", |app, ctx| {
            new_version(app);
            app.client.ui.management.settings = true;
            let width = 2400.0;
            paint(app, ctx, width, vec![]).1.drop_without_applying_deltas();
            select_updates(app, ctx, width);
            let text = drawn(app, ctx, width);
            let current = format!("This version: {}", env!("CARGO_PKG_VERSION"));
            for line in [current.as_str(), "New version: 9.0.0", "Release notes"] {
                assert!(text.iter().any(|t| t == line), "{line}: {text:?}");
            }
        });
    }

    #[test]
    fn with_no_way_to_install_the_bar_only_hints_and_settings_says_why() {
        with_app("app::update::tests::with_no_way_to_install_the_bar_only_hints_and_settings_says_why", |app, ctx| {
            new_version(app);
            app.runtime.update.unavailable = Some("This build has no release signing key.".into());
            let width = 2400.0;
            let text = drawn(app, ctx, width);
            assert!(text.iter().any(|t| t == "9.0.0 available"), "{text:?}");
            assert!(!text.iter().any(|t| t == "Update"), "no button that can only fail: {text:?}");
            app.client.ui.management.settings = true;
            paint(app, ctx, width, vec![]).1.drop_without_applying_deltas();
            select_updates(app, ctx, width);
            let text = drawn(app, ctx, width);
            assert!(text.iter().any(|t| t == "This build has no release signing key."), "{text:?}");
        });
    }

    #[test]
    fn checks_run_only_when_on_and_a_change_of_config_forgets_what_was_found() {
        with_app(
            "app::update::tests::checks_run_only_when_on_and_a_change_of_config_forgets_what_was_found",
            |app, _| {
                let release = Release::signed();
                app.runtime.update.test_mode = Some(signed_feed_with(&release));
                app.runtime.config.update.check = false;
                app.runtime.update.next_check = Some(Instant::now());
                assert!(app.runtime.poll_update(), "the config changed");
                app.runtime.update.next_check = Some(Instant::now());
                app.runtime.poll_update();
                assert!(app.runtime.update.check.is_none(), "[update] check = false");

                app.runtime.config.update.check = true;
                app.runtime.poll_update();
                app.runtime.update.next_check = Some(Instant::now());
                app.runtime.poll_update();
                let deadline = Instant::now() + Duration::from_secs(30);
                while app.runtime.update.check.is_some() {
                    assert!(Instant::now() < deadline, "the check never answered");
                    std::thread::sleep(Duration::from_millis(5));
                    app.runtime.poll_update();
                }
                assert_eq!(app.runtime.update_info().latest.as_deref(), Some("9.0.0"));
                assert_eq!(app.runtime.update_info().notes.as_deref(), Some("https://example.com/notes/9.0.0"));

                app.runtime.config.update.check = false;
                app.runtime.poll_update();
                assert_eq!(app.runtime.update_info().latest, None, "with checks off nothing new shows");
            },
        );
    }

    /// The countdown has run out.
    fn countdown_over(app: &mut Desktop<'_>) {
        app.runtime.update.restart_at = Some(Instant::now());
    }

    /// Runs the job the button started to its end.
    fn finish_job(app: &mut Desktop<'_>) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while app.runtime.update.installing() {
            assert!(Instant::now() < deadline, "the update never finished");
            std::thread::sleep(Duration::from_millis(5));
            app.runtime.read_update_job();
        }
    }

    #[test]
    fn a_failed_update_says_why_and_stays_on_the_old_version() {
        with_app("app::update::tests::a_failed_update_says_why_and_stays_on_the_old_version", |app, _| {
            let release = Release::signed();
            let mode = signed_feed_failing(&release);
            app.runtime.update.found = Some(mode.check().unwrap().unwrap());
            app.runtime.update.test_mode = Some(mode);
            app.runtime.start_update();
            assert!(matches!(app.runtime.update.phase, UpdatePhase::Working { .. }));
            assert!(!app.runtime.restart_blockers().is_empty(), "a restart waits for the job");
            finish_job(app);
            let UpdatePhase::Failed { error } = &app.runtime.update.phase else {
                panic!("{:?}", app.runtime.update.phase)
            };
            assert!(error.contains("not signed"), "{error}");
            assert!(app.runtime.toast.as_ref().unwrap().0.contains("not signed"), "said where the user is looking");
            assert!(!app.runtime.restarting);
            assert_eq!(std::fs::read_to_string(&release.installed).unwrap(), crate::update::test_support::OLD);
            // The Update button is there to try again.
            assert_eq!(app.runtime.update_info().latest.as_deref(), Some("9.0.0"));
        });
    }

    #[test]
    fn a_finished_update_waits_for_the_agents_and_cancel_stops_the_wait() {
        with_app("app::update::tests::a_finished_update_waits_for_the_agents_and_cancel_stops_the_wait", |app, ctx| {
            let release = Release::signed();
            let mode = signed_feed_with(&release);
            app.runtime.update.found = Some(mode.check().unwrap().unwrap());
            app.runtime.update.test_mode = Some(mode);
            app.runtime.sessions =
                vec![session("worker", AgentKind::Claude, AgentStatus::Working, &app.runtime.launch_dir)];
            app.runtime.start_update();
            finish_job(app);
            assert_eq!(std::fs::read_to_string(&release.installed).unwrap(), crate::update::test_support::NEW);

            let target =
                || -> anyhow::Result<crate::restart::Target> { panic!("nothing restarts while the agent works") };
            for _ in 0..3 {
                app.runtime.restart_when_clear(target);
                assert!(!app.runtime.restarting, "never forced");
            }
            let UpdatePhase::WaitingForAgents { blockers } = &app.runtime.update.phase else {
                panic!("{:?}", app.runtime.update.phase)
            };
            assert!(blockers[0].contains("worker"), "{blockers:?}");
            let width = 2400.0;
            let text = drawn(app, ctx, width);
            assert!(text.iter().any(|t| t == "Restarts when agents finish"), "{text:?}");

            // Cancel keeps what is installed and stays on this version, with a way back.
            click(app, ctx, width, "Cancel");
            assert_eq!(app.runtime.update.phase, UpdatePhase::Installed { version: "9.0.0".into() });
            app.runtime.poll_update();
            let text = drawn(app, ctx, width);
            assert!(text.iter().any(|t| t == "Restart to update"), "{text:?}");
            app.runtime.sessions[0].status = AgentStatus::Idle;
            app.runtime.poll_update();
            assert!(!app.runtime.restarting, "a cancelled wait does not restart when the agents finish");

            // Pressing again waits again, and with no agent working it restarts, through the safe restart.
            app.runtime.sessions[0].status = AgentStatus::Working;
            click(app, ctx, width, "Restart to update");
            assert!(matches!(app.runtime.update.phase, UpdatePhase::WaitingForAgents { .. }));
            app.runtime.restart_when_clear(target);
            assert!(!app.runtime.restarting);

            app.runtime.sessions[0].status = AgentStatus::Idle;
            let program = paths::atomics_home().join("version-probe");
            crate::test_support::write_program(&program, "#!/bin/sh\ntest \"$1\" = --version\n");
            countdown_over(app);
            app.runtime.restart_when_clear(|| {
                Ok(crate::restart::Target {
                    executable: program.clone(),
                    command: vec![crate::test_support::program("true").into()],
                })
            });
            assert!(app.runtime.restarting);
            assert_eq!(Store::load(&paths::store_file()).restart.unwrap().reason.as_deref(), Some("update to 9.0.0"));
        });
    }

    #[test]
    fn a_restart_that_fails_keeps_the_new_version_installed_and_offers_it_again() {
        with_app(
            "app::update::tests::a_restart_that_fails_keeps_the_new_version_installed_and_offers_it_again",
            |app, _| {
                assert!(app.runtime.update_installed("9.0.0".into(), None).ok, "as atomics update asks");
                countdown_over(app);
                app.runtime.restart_when_clear(|| {
                    Ok(crate::restart::Target { executable: "/missing/atomics".into(), command: vec![] })
                });
                assert!(!app.runtime.restarting);
                assert_eq!(app.runtime.update.phase, UpdatePhase::Installed { version: "9.0.0".into() });
                assert!(app.runtime.toast.as_ref().unwrap().0.contains("Could not restart into 9.0.0"));
            },
        );
    }

    #[test]
    fn the_host_publishes_its_version_and_what_it_found_to_clients() {
        with_app("app::update::tests::the_host_publishes_its_version_and_what_it_found_to_clients", |app, _| {
            new_version(app);
            let info = app.runtime.host_info().update.expect("published");
            assert_eq!(info.current, env!("CARGO_PKG_VERSION"));
            assert_eq!(info.latest.as_deref(), Some("9.0.0"));
            assert_eq!(info.notes.as_deref(), Some("https://example.com/notes"));
        });
    }

    #[test]
    fn the_button_leaves_the_keyboard_with_the_terminal_and_agent_chords_keep_working() {
        with_app(
            "app::update::tests::the_button_leaves_the_keyboard_with_the_terminal_and_agent_chords_keep_working",
            |app, ctx| keyboard_stays(app, ctx, 2400.0),
        );
    }

    #[test]
    fn the_button_folded_under_actions_leaves_the_keyboard_and_agent_chords_alone_too() {
        with_app(
            "app::update::tests::the_button_folded_under_actions_leaves_the_keyboard_and_agent_chords_alone_too",
            |app, ctx| keyboard_stays(app, ctx, 900.0),
        );
    }

    /// Two live agents, the first with the keyboard; the button appears and is pressed, in the bar or through the
    /// Actions menu where `width` folds it: the terminal gets the keys after it, and Ctrl+2 still goes to the second.
    fn keyboard_stays(app: &mut Desktop<'_>, ctx: &egui::Context, width: f32) {
        let dir = crate::test_support::tempdir().unwrap();
        let (first, second) = (dir.path().join("first"), dir.path().join("second"));
        for (id, root) in [("first", &first), ("second", &second)] {
            std::fs::create_dir_all(root).unwrap();
            let terminal = Terminal::spawn(
                app.runtime.wake.clone(),
                app.runtime.theme.terminal_colors(),
                "/bin/sh".into(),
                vec!["-c".into(), "stty raw -echo; printf ready; exec cat > input.bin".into()],
                root,
                HashMap::new(),
                100,
            )
            .unwrap();
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(3));
            while !terminal.screen_text(24).contains("ready") || !root.join("input.bin").exists() {
                assert!(Instant::now() < deadline, "terminal did not start");
                std::thread::sleep(Duration::from_millis(5));
            }
            let mut agent = session(id, AgentKind::Generic, AgentStatus::Idle, root);
            agent.terminal = Some(terminal);
            app.runtime.sessions.push(agent);
        }
        app.runtime.active = Some("first".into());
        let run = |app: &mut Desktop<'_>, events| paint(app, ctx, width, events).1.drop_without_applying_deltas();
        app.client.focus_terminal = app.runtime.active.clone();
        run(app, vec![]);
        run(app, vec![]);
        let holder = ctx.memory(|m| m.focused()).expect("the terminal has the keyboard");

        // The button or the dot appearing takes nothing, and pressing it opens nothing and gives the keyboard back.
        let release = Release::signed();
        app.runtime.update.test_mode = Some(signed_feed_failing(&release));
        app.runtime.update.found = Some(signed_feed_failing(&release).check().unwrap().unwrap());
        run(app, vec![]);
        assert_eq!(ctx.memory(|m| m.focused()), Some(holder), "appearing takes nothing");
        let folded = items(app, ctx, width).iter().any(|(item, _)| item == "Actions");
        assert_eq!(folded, width < 2000.0, "this test covers the bar it means to");
        if folded {
            assert!(dot(app, ctx, width), "folded, it marks the menu");
            assert_eq!(ctx.memory(|m| m.focused()), Some(holder), "the dot takes nothing");
            click(app, ctx, width, "Actions");
            // A menu sizes before its first paint.
            run(app, vec![]);
        }
        click(app, ctx, width, "Update");
        assert!(!app.client.ui.management.settings, "no window opens");
        run(app, vec![]);
        assert_eq!(ctx.memory(|m| m.focused()), Some(holder), "the terminal has the keyboard back");
        run(app, vec![egui::Event::Text("typed".into())]);
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(3));
        while std::fs::read(first.join("input.bin")).unwrap() != b"typed" {
            assert!(Instant::now() < deadline, "the terminal never got the keys");
            std::thread::sleep(Duration::from_millis(5));
        }

        // Ctrl+2 (Command on a Mac) still goes to the second agent's diff.
        let chord = if cfg!(target_os = "macos") { egui::Modifiers::MAC_CMD } else { egui::Modifiers::CTRL };
        let key = |pressed| egui::Event::Key {
            key: Key::Num2,
            physical_key: Some(Key::Num2),
            pressed,
            repeat: false,
            modifiers: chord,
        };
        run(app, vec![egui::Event::ModifiersChanged(chord), key(true)]);
        run(app, vec![key(false), egui::Event::ModifiersChanged(egui::Modifiers::NONE)]);
        run(app, vec![]);
        assert_eq!(app.runtime.active.as_deref(), Some("second"));
        assert!(ctx.memory(|m| m.has_focus(diff_view::focus_id())), "the keyboard stays with the diff");
        finish_job(app);
    }

    /// What holds the keyboard when the button is pressed.
    #[derive(Clone, Copy, Debug)]
    enum Holder {
        Diff,
        SettingsField,
        Nothing,
    }

    /// A click on the text `label` where the last frame drew it.
    fn click_text(app: &mut Desktop<'_>, ctx: &egui::Context, width: f32, label: &str) {
        let (_, output) = paint(app, ctx, width, vec![]);
        let at = output.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == label => Some(text.visual_bounding_rect().center()),
            _ => None,
        });
        output.drop_without_applying_deltas();
        let at = at.unwrap_or_else(|| panic!("{label} is not drawn"));
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        for events in [vec![egui::Event::PointerMoved(at)], vec![button(true)], vec![button(false)], vec![]] {
            paint(app, ctx, width, events).1.drop_without_applying_deltas();
        }
    }

    /// A key as the window gets it: pressed in one frame, released in the next.
    fn key(app: &mut Desktop<'_>, ctx: &egui::Context, width: f32, key: Key, modifiers: egui::Modifiers) {
        let event = |pressed| egui::Event::Key { key, physical_key: Some(key), pressed, repeat: false, modifiers };
        let press = vec![egui::Event::ModifiersChanged(modifiers), event(true)];
        paint(app, ctx, width, press).1.drop_without_applying_deltas();
        let release = vec![event(false), egui::Event::ModifiersChanged(egui::Modifiers::NONE)];
        paint(app, ctx, width, release).1.drop_without_applying_deltas();
    }

    /// `holder` has the keyboard, and Update appears and is pressed, in the bar or through the Actions menu where
    /// `width` folds it. The keyboard stays exactly where it was, no terminal behind gets a key, and Ctrl+2 still goes
    /// to the second agent's diff.
    fn keyboard_stays_with(app: &mut Desktop<'_>, ctx: &egui::Context, width: f32, holder: Holder) {
        let dir = crate::test_support::tempdir().unwrap();
        let (first, second) = two_agents(app, dir.path());
        let run = |app: &mut Desktop<'_>, events| paint(app, ctx, width, events).1.drop_without_applying_deltas();
        app.client.focus_terminal = app.runtime.active.clone();
        run(app, vec![]);
        run(app, vec![]);
        assert!(ctx.memory(|m| m.focused()).is_some(), "the terminal has the keyboard");
        match holder {
            Holder::Diff => {
                key(app, ctx, width, Key::ArrowDown, egui::Modifiers::SHIFT);
                assert!(ctx.memory(|m| m.has_focus(diff_view::focus_id())), "the diff has the keyboard");
            }
            Holder::SettingsField => {
                app.client.ui.management.settings = true;
                run(app, vec![]);
                run(app, vec![]);
                click_text(app, ctx, width, "Advanced");
                for _ in 0..8 {
                    run(app, vec![]);
                }
                click_text(app, ctx, width, "Find a setting");
                run(app, vec![egui::Event::Text("x".into())]);
                assert_eq!(app.client.ui.management.filter, "x", "the field has the keyboard");
            }
            Holder::Nothing => {
                let held = ctx.memory(|m| m.focused()).unwrap();
                ctx.memory_mut(|m| m.surrender_focus(held));
                run(app, vec![]);
                assert_eq!(ctx.memory(|m| m.focused()), None);
            }
        }
        let held = ctx.memory(|m| m.focused());

        let release = Release::signed();
        app.runtime.update.test_mode = Some(signed_feed_failing(&release));
        app.runtime.update.found = Some(signed_feed_failing(&release).check().unwrap().unwrap());
        run(app, vec![]);
        assert_eq!(ctx.memory(|m| m.focused()), held, "{holder:?}, {width}: appearing takes nothing");
        if items(app, ctx, width).iter().any(|(item, _)| item == "Actions") {
            click(app, ctx, width, "Actions");
            // A menu sizes before its first paint.
            run(app, vec![]);
        }
        click(app, ctx, width, "Update");
        assert!(matches!(app.runtime.update.phase, UpdatePhase::Working { .. } | UpdatePhase::Failed { .. }));
        run(app, vec![]);
        run(app, vec![]);
        assert_eq!(ctx.memory(|m| m.focused()), held, "{holder:?}, {width}: the keyboard stays where it was");

        // The keys after it go where they went before it.
        match holder {
            Holder::Diff => {
                key(app, ctx, width, Key::PageDown, egui::Modifiers::NONE);
                key(app, ctx, width, Key::ArrowDown, egui::Modifiers::NONE);
                assert!(ctx.memory(|m| m.has_focus(diff_view::focus_id())), "{holder:?}, {width}: stays with the diff");
            }
            Holder::SettingsField => {
                run(app, vec![egui::Event::Text("y".into())]);
                assert_eq!(app.client.ui.management.filter, "xy", "{holder:?}, {width}: typing stays in the field");
            }
            Holder::Nothing => {
                key(app, ctx, width, Key::PageDown, egui::Modifiers::NONE);
                run(app, vec![egui::Event::Text("typed".into())]);
                assert_eq!(ctx.memory(|m| m.focused()), None, "{holder:?}, {width}: nothing took it");
            }
        }

        // Ctrl+2 (Command on a Mac) still goes to the second agent's diff.
        let chord = if cfg!(target_os = "macos") { egui::Modifiers::MAC_CMD } else { egui::Modifiers::CTRL };
        key(app, ctx, width, Key::Num2, chord);
        run(app, vec![]);
        assert_eq!(app.runtime.active.as_deref(), Some("second"), "{holder:?}, {width}");
        assert_eq!(app.runtime.diff_target().as_deref(), Some(second.as_path()), "{holder:?}, {width}");
        assert!(app.runtime.panel == PanelMode::Diff && app.runtime.diff_open, "{holder:?}, {width}: the diff shows");
        match holder {
            Holder::SettingsField => {
                assert_eq!(ctx.memory(|m| m.focused()), held, "{width}: the open window keeps its field's keyboard");
            }
            _ => assert!(ctx.memory(|m| m.has_focus(diff_view::focus_id())), "{holder:?}, {width}: with the diff"),
        }
        untouched(app, &[("first", &first), ("second", &second)], "end");
        finish_job(app);
    }

    #[test]
    fn an_agent_chord_on_the_frame_after_the_press_keeps_the_keyboard_it_moved() {
        with_app(
            "app::update::tests::an_agent_chord_on_the_frame_after_the_press_keeps_the_keyboard_it_moved",
            |app, ctx| {
                let width = 2400.0;
                let dir = crate::test_support::tempdir().unwrap();
                let (first, second) = two_agents(app, dir.path());
                let run =
                    |app: &mut Desktop<'_>, events| paint(app, ctx, width, events).1.drop_without_applying_deltas();
                app.client.focus_terminal = app.runtime.active.clone();
                run(app, vec![]);
                run(app, vec![]);
                let release = Release::signed();
                app.runtime.update.test_mode = Some(signed_feed_failing(&release));
                app.runtime.update.found = Some(signed_feed_failing(&release).check().unwrap().unwrap());
                let at =
                    items(app, ctx, width).iter().find(|(item, _)| item == "Update").expect("in the bar").1.center();
                let button = |pressed| egui::Event::PointerButton {
                    pos: at,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                };
                run(app, vec![egui::Event::PointerMoved(at)]);
                run(app, vec![button(true)]);
                run(app, vec![button(false)]);
                assert!(matches!(app.runtime.update.phase, UpdatePhase::Working { .. } | UpdatePhase::Failed { .. }));

                // With nothing in between.
                let chord = if cfg!(target_os = "macos") { egui::Modifiers::MAC_CMD } else { egui::Modifiers::CTRL };
                key(app, ctx, width, Key::Num2, chord);
                run(app, vec![]);
                assert_eq!(app.runtime.active.as_deref(), Some("second"));
                assert_eq!(app.runtime.diff_target().as_deref(), Some(second.as_path()));
                assert!(ctx.memory(|m| m.has_focus(diff_view::focus_id())), "the keyboard stays with the diff");
                untouched(app, &[("first", &first), ("second", &second)], "end");
                finish_job(app);
            },
        );
    }

    #[test]
    fn the_button_leaves_the_keyboard_in_the_diff() {
        with_app("app::update::tests::the_button_leaves_the_keyboard_in_the_diff", |app, ctx| {
            keyboard_stays_with(app, ctx, 2400.0, Holder::Diff)
        });
    }

    #[test]
    fn the_button_folded_under_actions_leaves_the_keyboard_in_the_diff() {
        with_app("app::update::tests::the_button_folded_under_actions_leaves_the_keyboard_in_the_diff", |app, ctx| {
            keyboard_stays_with(app, ctx, 900.0, Holder::Diff)
        });
    }

    #[test]
    fn the_button_leaves_the_keyboard_in_a_settings_field() {
        with_app("app::update::tests::the_button_leaves_the_keyboard_in_a_settings_field", |app, ctx| {
            keyboard_stays_with(app, ctx, 2400.0, Holder::SettingsField)
        });
    }

    #[test]
    fn the_button_folded_under_actions_leaves_the_keyboard_in_a_settings_field() {
        with_app(
            "app::update::tests::the_button_folded_under_actions_leaves_the_keyboard_in_a_settings_field",
            |app, ctx| keyboard_stays_with(app, ctx, 900.0, Holder::SettingsField),
        );
    }

    #[test]
    fn the_button_gives_nothing_the_keyboard_when_nothing_held_it() {
        with_app("app::update::tests::the_button_gives_nothing_the_keyboard_when_nothing_held_it", |app, ctx| {
            keyboard_stays_with(app, ctx, 2400.0, Holder::Nothing)
        });
    }

    #[test]
    fn the_button_folded_under_actions_gives_nothing_the_keyboard_when_nothing_held_it() {
        with_app(
            "app::update::tests::the_button_folded_under_actions_gives_nothing_the_keyboard_when_nothing_held_it",
            |app, ctx| keyboard_stays_with(app, ctx, 900.0, Holder::Nothing),
        );
    }

    /// The wait for agents says `expected` holds the restart back, and nothing restarts.
    fn holds_the_restart(app: &mut Desktop<'_>, expected: &str) {
        let target = || -> anyhow::Result<crate::restart::Target> { panic!("the restart went over {expected}") };
        app.runtime.restart_when_clear(target);
        assert!(!app.runtime.restarting, "{expected}");
        let UpdatePhase::WaitingForAgents { blockers } = &app.runtime.update.phase else {
            panic!("{expected}: {:?}", app.runtime.update.phase)
        };
        assert!(blockers.iter().any(|blocker| blocker.contains(expected)), "{expected}: {blockers:?}");
    }

    /// A live terminal running `script` with `text` as its `$0`, once `shown` is on its screen.
    fn terminal_running(app: &Desktop<'_>, root: &Path, program: &str, args: &[&str], shown: &str) -> Terminal {
        let terminal = Terminal::spawn(
            app.runtime.wake.clone(),
            app.runtime.theme.terminal_colors(),
            program.into(),
            args.iter().map(|arg| arg.to_string()).collect(),
            root,
            HashMap::from([("PS1".to_string(), "ready> ".to_string())]),
            100,
        )
        .unwrap();
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
        while !terminal.screen_text(24).contains(shown) {
            assert!(Instant::now() < deadline, "{shown} never showed: {}", terminal.screen_text(24));
            std::thread::sleep(Duration::from_millis(5));
        }
        terminal
    }

    /// A program that prints `text` as the last line of its screen and stays.
    fn footer(app: &Desktop<'_>, root: &Path, text: &str) -> Terminal {
        let script = "printf '%s\\n' \"$0\"; exec sleep 300";
        terminal_running(app, root, "/bin/sh", &["-c", script, text], text)
    }

    #[test]
    fn a_working_chief_holds_the_update_restart_back() {
        with_app("app::update::tests::a_working_chief_holds_the_update_restart_back", |app, _| {
            assert!(app.runtime.update_installed("9.0.0".into(), None).ok);
            let mut chief = session("chief", AgentKind::Claude, AgentStatus::Working, &app.runtime.launch_dir);
            chief.chief = true;
            app.runtime.sessions = vec![chief];
            holds_the_restart(app, "chief");
        });
    }

    #[test]
    fn an_agent_whose_screen_is_still_in_a_turn_holds_the_update_restart_back() {
        with_app(
            "app::update::tests::an_agent_whose_screen_is_still_in_a_turn_holds_the_update_restart_back",
            |app, _| {
                assert!(app.runtime.update_installed("9.0.0".into(), None).ok);
                let dir = crate::test_support::tempdir().unwrap();
                // Where a sandboxed agent's report arrives before its turn ends.
                let mut worker = session("worker", AgentKind::Claude, AgentStatus::Done, dir.path());
                worker.screen_busy = true;
                worker.terminal = Some(footer(app, dir.path(), "esc to interrupt"));
                app.runtime.sessions = vec![worker];
                holds_the_restart(app, "worker");
            },
        );
    }

    #[test]
    fn an_idle_agent_with_background_work_holds_the_update_restart_back() {
        with_app("app::update::tests::an_idle_agent_with_background_work_holds_the_update_restart_back", |app, _| {
            assert!(app.runtime.update_installed("9.0.0".into(), None).ok);
            let dir = crate::test_support::tempdir().unwrap();
            let mut worker = session("worker", AgentKind::Claude, AgentStatus::Idle, dir.path());
            worker.terminal = Some(footer(app, dir.path(), "  ⏵⏵ auto mode on · 1 shell · ← 1 agent"));
            app.runtime.sessions = vec![worker];
            holds_the_restart(app, "1 shell");
        });
    }

    #[test]
    fn a_shell_running_a_command_holds_the_update_restart_back() {
        with_app("app::update::tests::a_shell_running_a_command_holds_the_update_restart_back", |app, _| {
            assert!(app.runtime.update_installed("9.0.0".into(), None).ok);
            let dir = crate::test_support::tempdir().unwrap();
            let terminal = terminal_running(app, dir.path(), "/bin/sh", &["-i"], "ready>");
            assert!(!terminal.foreground_process(), "at its prompt");
            terminal.write(b"sleep 300\r".to_vec());
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
            while !terminal.foreground_process() {
                assert!(Instant::now() < deadline, "the shell never ran it: {}", terminal.screen_text(24));
                std::thread::sleep(Duration::from_millis(5));
            }
            let mut shell = session("shell", AgentKind::Shell, AgentStatus::Idle, dir.path());
            shell.terminal = Some(terminal);
            app.runtime.sessions = vec![shell];
            holds_the_restart(app, "shell");

            // Back at its prompt it holds nothing. What is typed there is what the countdown is for.
            let terminal = app.runtime.sessions[0].terminal.as_ref().unwrap();
            terminal.write(b"\x03".to_vec());
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
            while terminal.foreground_process() {
                assert!(Instant::now() < deadline, "the command did not stop: {}", terminal.screen_text(24));
                std::thread::sleep(Duration::from_millis(5));
            }
            app.runtime.restart_when_clear(|| panic!("the countdown comes first"));
            assert_eq!(app.runtime.update.phase, UpdatePhase::RestartsIn { seconds: COUNTDOWN.as_secs() });
        });
    }

    #[test]
    fn a_running_editor_holds_the_update_restart_back() {
        with_app("app::update::tests::a_running_editor_holds_the_update_restart_back", |app, _| {
            assert!(app.runtime.update_installed("9.0.0".into(), None).ok);
            let dir = crate::test_support::tempdir().unwrap();
            let editor = footer(app, dir.path(), "unsaved");
            app.runtime.editors.insert(dir.path().to_path_buf(), editor);
            holds_the_restart(app, "editor");

            // Quit, it holds nothing.
            let editor = &app.runtime.editors[dir.path()];
            // SAFETY: a signal to the process this terminal started.
            unsafe { libc::kill(editor.process_id() as i32, libc::SIGTERM) };
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
            while !editor.exited() {
                assert!(Instant::now() < deadline, "the editor did not stop");
                std::thread::sleep(Duration::from_millis(5));
            }
            app.runtime.restart_when_clear(|| panic!("the countdown comes first"));
            assert_eq!(app.runtime.update.phase, UpdatePhase::RestartsIn { seconds: COUNTDOWN.as_secs() });
        });
    }

    #[test]
    fn with_nothing_in_the_way_the_window_counts_down_and_anything_new_starts_it_over() {
        with_app(
            "app::update::tests::with_nothing_in_the_way_the_window_counts_down_and_anything_new_starts_it_over",
            |app, _| {
                let whole = UpdatePhase::RestartsIn { seconds: COUNTDOWN.as_secs() };
                let target =
                    || -> anyhow::Result<crate::restart::Target> { panic!("the window closed during the countdown") };
                assert!(app.runtime.update_installed("9.0.0".into(), None).ok);
                app.runtime.restart_when_clear(target);
                assert_eq!(app.runtime.update.phase, whole);
                assert!(!app.runtime.restarting);

                // An agent starts a turn: back to waiting, and the next countdown is a whole one.
                app.runtime.update.restart_at = Some(Instant::now() + Duration::from_millis(1));
                app.runtime.sessions =
                    vec![session("worker", AgentKind::Claude, AgentStatus::Working, &app.runtime.launch_dir)];
                holds_the_restart(app, "worker");
                assert!(app.runtime.update.restart_at.is_none());
                app.runtime.sessions[0].status = AgentStatus::Idle;
                app.runtime.restart_when_clear(target);
                assert_eq!(app.runtime.update.phase, whole);

                // It runs out and restarts.
                let program = paths::atomics_home().join("version-probe");
                crate::test_support::write_program(&program, "#!/bin/sh\ntest \"$1\" = --version\n");
                countdown_over(app);
                app.runtime.restart_when_clear(|| {
                    Ok(crate::restart::Target {
                        executable: program.clone(),
                        command: vec![crate::test_support::program("true").into()],
                    })
                });
                assert!(app.runtime.restarting);
            },
        );
    }

    #[test]
    fn cancel_stops_the_countdown_and_pressing_again_counts_from_the_start() {
        with_app(
            "app::update::tests::cancel_stops_the_countdown_and_pressing_again_counts_from_the_start",
            |app, _| {
                let target = || -> anyhow::Result<crate::restart::Target> { panic!("nothing restarts here") };
                assert!(app.runtime.update_installed("9.0.0".into(), None).ok);
                app.runtime.restart_when_clear(target);
                countdown_over(app);
                app.runtime.cancel_update_wait();
                assert_eq!(app.runtime.update.phase, UpdatePhase::Installed { version: "9.0.0".into() });
                app.runtime.poll_update();
                assert!(!app.runtime.restarting, "a cancelled countdown does not run on");

                app.runtime.start_update();
                assert!(matches!(app.runtime.update.phase, UpdatePhase::WaitingForAgents { .. }));
                app.runtime.restart_when_clear(target);
                assert_eq!(app.runtime.update.phase, UpdatePhase::RestartsIn { seconds: COUNTDOWN.as_secs() });
            },
        );
    }

    #[test]
    fn the_countdown_shows_at_every_width_and_its_cancel_leaves_the_keyboard_and_agent_chords_alone() {
        with_app(
            "app::update::tests::the_countdown_shows_at_every_width_and_its_cancel_leaves_the_keyboard_and_agent_chords_alone",
            |app, ctx| {
                let dir = crate::test_support::tempdir().unwrap();
                let (first, second) = two_agents(app, dir.path());
                app.runtime.update.version = Some("9.0.0".into());
                for width in [480.0, 700.0, 900.0, 1500.0, 2400.0] {
                    app.client.focus_terminal = app.runtime.active.clone();
                    let run = |app: &mut Desktop<'_>| paint(app, ctx, width, vec![]).1.drop_without_applying_deltas();
                    run(app);
                    run(app);
                    let holder = ctx.memory(|m| m.focused()).expect("the terminal has the keyboard");

                    app.runtime.update.phase = UpdatePhase::RestartsIn { seconds: 7 };
                    // An area sizes before its first paint.
                    run(app);
                    let row = items(app, ctx, width);
                    let window = Rect::from_min_size(Pos2::ZERO, vec2(width, 800.0));
                    for name in ["countdown", "Cancel"] {
                        let rect =
                            row.iter().find(|(item, _)| item == name).unwrap_or_else(|| panic!("{width}: {name}"));
                        assert!(window.contains_rect(rect.1), "{width}: {name} at {:?}", rect.1);
                    }
                    let text = drawn(app, ctx, width);
                    assert!(text.iter().any(|t| t == "Restarting in 7 s to update"), "{width}: {text:?}");
                    assert_eq!(ctx.memory(|m| m.focused()), Some(holder), "{width}: appearing takes nothing");

                    click(app, ctx, width, "Cancel");
                    assert_eq!(app.runtime.update.phase, UpdatePhase::Installed { version: "9.0.0".into() }, "{width}");
                    run(app);
                    assert_eq!(ctx.memory(|m| m.focused()), Some(holder), "{width}: the keyboard stays");
                }

                // Ctrl+2 (Command on a Mac) goes to the second agent's diff while it counts, and after Cancel too.
                let chord = if cfg!(target_os = "macos") { egui::Modifiers::MAC_CMD } else { egui::Modifiers::CTRL };
                let width = 900.0;
                app.runtime.update.phase = UpdatePhase::RestartsIn { seconds: 7 };
                key(app, ctx, width, Key::Num2, chord);
                paint(app, ctx, width, vec![]).1.drop_without_applying_deltas();
                assert_eq!(app.runtime.active.as_deref(), Some("second"));
                assert_eq!(app.runtime.diff_target().as_deref(), Some(second.as_path()));
                assert!(ctx.memory(|m| m.has_focus(diff_view::focus_id())), "with the diff");
                click(app, ctx, width, "Cancel");
                paint(app, ctx, width, vec![]).1.drop_without_applying_deltas();
                assert!(ctx.memory(|m| m.has_focus(diff_view::focus_id())), "still with the diff");
                key(app, ctx, width, Key::Num1, chord);
                paint(app, ctx, width, vec![]).1.drop_without_applying_deltas();
                assert_eq!(app.runtime.active.as_deref(), Some("first"));
                assert_eq!(app.runtime.diff_target().as_deref(), Some(first.as_path()));
                untouched(app, &[("first", &first), ("second", &second)], "end");
            },
        );
    }
}
