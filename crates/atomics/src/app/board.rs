use super::*;

impl State {
    pub(super) fn toggle_plan(&mut self) {
        self.window.request_repaint();
        if self.panel == PanelMode::Plan && self.diff_open {
            self.panel = PanelMode::Diff;
        } else {
            self.panel = PanelMode::Plan;
            self.diff_open = true;
        }
    }
}

pub(super) use atomics_ui::status::task_color;
