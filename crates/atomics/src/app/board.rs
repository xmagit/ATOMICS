use super::*;

impl State {
    pub(super) fn toggle_plan(&mut self) {
        self.toggle_view(PanelMode::Plan);
    }

    /// The view switcher's segment, and the keys that toggle a view: show `view` in the panel (the diff
    /// keeps the editor if it shows), or hide the panel when it shows `view` already. The window remembers
    /// the view it showed last ([`Store::panel_view`]).
    pub(super) fn toggle_view(&mut self, view: PanelMode) {
        self.window.request_repaint();
        let shown = atomics_ui::switcher::view_of(self.panel);
        if self.diff_open && shown == view {
            self.diff_open = false;
            return;
        }
        if shown != view {
            self.panel = view;
        }
        self.diff_open = true;
        if view == PanelMode::Tickets {
            // The panel takes the keyboard where it opens. The window hands this on to the reader it draws.
            let panel = &mut self.tickets.panel;
            panel.focus_query = panel.view == super::tickets_panel::TicketsView::Find;
            panel.focus = !panel.focus_query;
        }
        let name = match view {
            PanelMode::Plan => "board",
            PanelMode::Tickets => "tickets",
            _ => "diff",
        };
        if self.store.panel_view.as_deref() != Some(name) {
            self.store.panel_view = Some(name.into());
            if let Err(error) = self.store.save_later(&paths::store_file()) {
                eprintln!("atomics: could not remember the view: {error}");
            }
        }
    }
}
