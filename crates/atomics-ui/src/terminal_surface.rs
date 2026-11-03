//! The hook through which the views draw a session's terminal: the fleet's tiles, and any other view that
//! holds one. The platform implements it: the desktop with its own terminal view over the session's
//! process, a browser with the screen the host streams.

use egui::Rect;

use crate::terminal::Metrics;
use crate::theme::Palette;

/// Draws a session's terminal where a view puts it.
pub trait TerminalSurface {
    /// Draws the terminal of the session at `index` of [`Backend::session_list`](crate::Backend::session_list),
    /// whose id is `session`, into `rect` under `id`, with `metrics` and `palette`, and feeds it the
    /// keyboard and the pointer while it has focus. Views call it only for sessions running inside ATOMICS
    /// ([`SessionRow::outside`](crate::SessionRow::outside) is false).
    #[allow(clippy::too_many_arguments)]
    fn show(
        &mut self,
        ui: &mut egui::Ui,
        index: usize,
        session: &str,
        rect: Rect,
        id: egui::Id,
        metrics: &Metrics,
        palette: &Palette,
    ) -> egui::Response;
}
