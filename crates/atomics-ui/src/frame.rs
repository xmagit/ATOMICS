//! What a frame keeps from the one before. A frame is drawn for every move of the pointer, and most of them show the
//! same rows: what is dear to make of them is made again only once the backend shows something else
//! ([`Backend::view_revision`]).
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::{Backend, RecentSession, SessionRow};

#[derive(Clone)]
struct Kept<T> {
    pass: u64,
    revision: Option<u64>,
    of: u64,
    value: T,
}

/// What `make` gives for `of`, made once a frame, and once for all the frames that show the same rows.
pub fn kept<T: Clone + Send + Sync + 'static>(
    ctx: &egui::Context,
    backend: &dyn Backend,
    key: &'static str,
    of: impl Hash,
    make: impl FnOnce() -> T,
) -> T {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    of.hash(&mut hasher);
    let (of, pass, revision) = (hasher.finish(), ctx.cumulative_pass_nr(), backend.view_revision());
    let id = egui::Id::unique(key);
    if let Some(kept) = ctx.data(|d| d.get_temp::<Kept<T>>(id))
        && kept.of == of
        && (kept.pass == pass || (revision.is_some() && kept.revision == revision))
    {
        return kept.value;
    }
    let value = make();
    ctx.data_mut(|d| d.insert_temp(id, Kept { pass, revision, of, value: value.clone() }));
    value
}

/// Every session, as [`Backend::session_list`] gives them.
pub fn sessions(ctx: &egui::Context, backend: &dyn Backend) -> Arc<Vec<SessionRow>> {
    kept(ctx, backend, "atomics-frame-sessions", (), || backend.sessions())
}

/// The closed sessions to resume, as [`Backend::recent_sessions`] gives them.
pub fn recent(ctx: &egui::Context, backend: &dyn Backend) -> Arc<Vec<RecentSession>> {
    kept(ctx, backend, "atomics-frame-recent", (), || Arc::new(backend.recent_sessions()))
}

/// How far out of view a row still is laid out.
const MARGIN: f32 = 40.0;

/// Lays out a row of a list that scrolls, down or to the right, only where it shows. Out of view it takes the space it
/// took when it last was laid out that wide, and `None` is what it gives. One that has the keyboard, or that the
/// pointer drags, stays.
pub fn in_view<R>(
    ui: &mut egui::Ui,
    id: impl Hash + std::fmt::Debug,
    contents: impl FnOnce(&mut egui::Ui) -> R,
) -> Option<R> {
    let key = egui::Id::unique(("atomics-in-view", &id));
    let horizontal = ui.layout().is_horizontal();
    let start = |ui: &egui::Ui| if horizontal { ui.cursor().left() } else { ui.cursor().top() };
    let (at, across) = (start(ui), if horizontal { ui.available_height() } else { ui.available_width() });
    let kept = ui.data(|d| d.get_temp::<(f32, f32)>(key));
    if let Some((_, extent)) = kept.filter(|(was, _)| *was == across && !ui.is_sizing_pass()) {
        let clip = if horizontal { ui.clip_rect().x_range() } else { ui.clip_rect().y_range() };
        let out = at > clip.max + MARGIN || at + extent < clip.min - MARGIN;
        if out && !engaged(ui.ctx(), key) {
            // What follows keeps its ids.
            ui.skip_ahead_auto_ids(1);
            ui.add_space(extent);
            return None;
        }
    }
    let scope = ui.push_id(id, contents);
    let now = (across, start(ui) - at);
    let rect = scope.response.rect;
    let holds = |id: Option<egui::Id>| {
        id.and_then(|id| ui.ctx().read_response(id)).is_some_and(|response| rect.contains(response.rect.center()))
    };
    let held = holds(ui.memory(|m| m.focused())) || holds(ui.ctx().dragged_id());
    ui.data_mut(|d| {
        if kept != Some(now) {
            d.insert_temp(key, now);
        }
        if held {
            d.insert_temp(key, Engaged);
        } else {
            d.remove::<Engaged>(key);
        }
    });
    Some(scope.inner)
}

/// Whether a row `height` high shows, in a list that scrolls down. Where it does not, its space is taken and what follows
/// keeps its ids, as if one child had been drawn.
pub fn row_in_view(ui: &mut egui::Ui, height: f32) -> bool {
    let (top, visible) = (ui.next_widget_position().y, ui.clip_rect());
    if top + height >= visible.top() && top <= visible.bottom() {
        return true;
    }
    ui.skip_ahead_auto_ids(1);
    ui.add_space(height + ui.spacing().item_spacing.y);
    false
}

/// What a row with the keyboard or the pointer in it leaves behind.
#[derive(Clone)]
struct Engaged;

fn engaged(ctx: &egui::Context, key: egui::Id) -> bool {
    ctx.data(|d| d.get_temp::<Engaged>(key)).is_some()
}
