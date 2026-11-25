//! The desktop's window frame: `atomics_ui::shell` lays it out and draws it over the in-process backend
//! with the desktop's terminals ([`Desktop::draw_terminals`]). The hooks here draw what only this host has:
//! the ticket integration, the GitHub counter and the VM indicator in the top bar. Settings, Remote,
//! Project, USAGE/load, the chief with its trace, tickets, the editor and pull-request diffs are shared.
//! The ticket hooks retain the fallback for a backend without the shared reader state.

use atomics_ui::chief::VoiceState;
use atomics_ui::shell::Platform;
use atomics_ui::terminal_surface::TerminalSurface;

use super::*;

impl Platform for Desktop<'_> {
    fn draw<R>(
        &mut self,
        draw: impl FnOnce(&dyn Backend, &mut atomics_ui::Client, &mut dyn TerminalSurface) -> R,
    ) -> R {
        // ctl and atomics:// links still address the desktop's navigation adapter. Import a new
        // request once; ordinary reader selection and history remain in the shared client.
        let panel = &mut self.runtime.tickets.panel;
        if self.runtime.panel == PanelMode::Tickets
            && (std::mem::take(&mut panel.focus_query) | std::mem::take(&mut panel.focus))
        {
            self.client.ui.tickets.take_keyboard();
        }
        if self.runtime.panel == PanelMode::Tickets && panel.seq != self.client.ui.tickets.external_revision {
            let ui = &mut self.client.ui;
            match panel.view {
                tickets_panel::TicketsView::Read => {
                    if let Some(read) = &panel.read {
                        ui.tickets.open(read.link.clone(), false, self.client.ctx.input(|i| i.time), &mut ui.commands);
                        ui.tickets.from_board = panel.from_board;
                    }
                }
                tickets_panel::TicketsView::Find => {
                    ui.tickets.found(&panel.query, panel.find.results.clone(), panel.find.from_chief)
                }
            }
            ui.tickets.external_revision = panel.seq;
        }
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

    fn top_bar_items(&mut self, ui: &mut egui::Ui, in_menu: bool) {
        self.host_top_bar_items(ui, in_menu);
    }

    fn panel_tickets(&mut self, ui: &mut egui::Ui, rect: Rect) {
        self.tickets_panel(ui, rect);
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
        self.apply_client_events(ui.ctx());
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
