//! The desktop's window frame: `atomics_ui::shell` lays it out and draws it over the in-process backend
//! with the desktop's terminals ([`Desktop::draw_terminals`]). The hooks here draw what only this host has:
//! the Tickets button and sign-in, the GitHub counter, the Remote indicator, the load gauge and Settings in
//! the top bar; USAGE and TICKETS in the sidebar; the chief's trace lane; the Tickets panel, a pull
//! request's diff and the editor in the right panel.

use atomics_ui::chief::VoiceState;
use atomics_ui::shell::Platform;
use atomics_ui::sidebar::section;
use atomics_ui::terminal_surface::TerminalSurface;

use super::*;

impl Platform for Desktop<'_> {
    fn draw<R>(
        &mut self,
        draw: impl FnOnce(&dyn Backend, &mut atomics_ui::Client, &mut dyn TerminalSurface) -> R,
    ) -> R {
        self.draw_terminals(draw)
    }

    fn voice_state(&self) -> Option<VoiceState> {
        Some(match &self.client.voice {
            Voice::Idle => VoiceState::Idle,
            Voice::Opening(_) => VoiceState::Opening,
            Voice::Recording(recorder) => VoiceState::Recording(recorder.elapsed().as_secs()),
            Voice::Transcribing => VoiceState::Transcribing,
        })
    }

    /// ctl's `list`, the shortcuts and the chief's dock read the layout the window last drew.
    fn laid_out(&mut self, layout: &Layout) {
        self.runtime.last_layout = Some(*layout);
    }

    fn byline(&self) -> Option<&'static str> {
        Some("an atomisk.ai project")
    }

    fn top_bar_items(&mut self, ui: &mut egui::Ui, in_menu: bool) {
        self.host_top_bar_items(ui, in_menu);
    }

    fn top_bar_load(&mut self, ui: &mut egui::Ui) {
        self.load_gauge(ui);
    }

    fn top_bar_settings(&mut self, ui: &mut egui::Ui) {
        self.settings_button(ui);
    }

    fn sidebar_usage(&mut self, ui: &mut egui::Ui) {
        let p = self.palette();
        section(ui, &p, "USAGE");
        self.runtime.usage.show(ui, &p);
        ui.separator();
    }

    fn sidebar_tickets(&mut self, ui: &mut egui::Ui) {
        let p = self.palette();
        self.tickets_section(ui, &p);
    }

    fn chief_window(&mut self, ui: &mut egui::Ui) {
        self.chief_content(ui);
    }

    fn panel_tickets(&mut self, ui: &mut egui::Ui, rect: Rect) {
        self.tickets_panel(ui, rect);
    }

    fn panel_pull_request(&mut self, ui: &mut egui::Ui, rect: Rect) -> bool {
        self.github_pr_panel(ui, rect)
    }

    fn panel_editor_file(&self, root: &Path) -> Option<String> {
        self.client.editor_file.get(root).cloned()
    }

    fn panel_editor(&mut self, ui: &mut egui::Ui, rect: Rect, root: &Path) {
        self.editor_panel(ui, rect, root);
    }
}

/// The pieces of the frame the tests draw on their own.
#[cfg(test)]
impl Desktop<'_> {
    /// The top bar, laid out by `atomics_ui::shell` with this host's items.
    pub(super) fn top_bar(&mut self, ui: &mut egui::Ui) {
        atomics_ui::shell::top_bar(ui, self);
    }

    pub(super) fn view_tabs(&mut self, ui: &mut egui::Ui) {
        self.draw(|backend, client| atomics_ui::tabs::view_tabs(ui, backend, client));
    }

    /// The right panel (`atomics_ui::shell::right_panel`), with this host's Tickets panel, pull request diff
    /// and editor.
    pub(super) fn diff_panel(&mut self, ui: &mut egui::Ui, rect: Rect) {
        atomics_ui::shell::right_panel(ui, rect, self);
    }

    /// The selected plan's board (`atomics_ui::board`).
    pub(super) fn plan_panel(&mut self, ui: &mut egui::Ui, rect: Rect) {
        atomics_ui::shell::plan_board(ui, rect, self);
    }

    pub(super) fn tiles(&mut self, ui: &mut egui::Ui, rect: Rect, layout: &Layout, tiled: &[usize]) {
        self.draw_terminals(|backend, client, terminals| {
            atomics_ui::fleet::tiles(ui, rect, layout, tiled, backend, client, terminals)
        });
    }
}
