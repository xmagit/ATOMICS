//! What the diff and the Tickets panel share: one keyboard focus, row-based scrolling by
//! the unmodified navigation keys, and stepping through a list of sections.

use egui::{Key, Sense};

/// The focus id of the panel beside the terminals. Shift+↓/↑ move it here from anywhere but
/// a text field or the editor; typing or Escape hands it back to the terminal.
pub fn focus_id() -> egui::Id {
    egui::Id::NULL.with("diff-keyboard")
}

/// Makes `rect` the keyboard target and keeps the arrows and Escape out of egui's own
/// focus movement while it has the focus.
pub fn focusable(ui: &egui::Ui, rect: egui::Rect) -> egui::Response {
    let focus = ui.interact(rect, focus_id(), Sense::focusable_noninteractive());
    if focus.has_focus() {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                focus_id(),
                egui::EventFilter { vertical_arrows: true, escape: true, ..Default::default() },
            );
        });
    }
    focus
}

/// The keys the panels navigate with.
pub const NAV_KEYS: [Key; 7] =
    [Key::ArrowUp, Key::ArrowDown, Key::PageUp, Key::PageDown, Key::Home, Key::End, Key::Enter];

/// Takes the unmodified navigation keys pressed this frame, only while the panel has the
/// focus: text fields, the editor and terminal modifiers are left alone.
pub fn take_keys(ui: &egui::Ui, keys: &[Key]) -> Vec<Key> {
    let mut taken = Vec::new();
    if !ui.memory(|m| m.has_focus(focus_id())) {
        return taken;
    }
    ui.input_mut(|i| {
        i.events.retain(|event| {
            let egui::Event::Key { key, pressed: true, modifiers, .. } = event else { return true };
            if !modifiers.is_none() || !keys.contains(key) {
                return true;
            }
            taken.push(*key);
            false
        });
    });
    taken
}

/// Consumes `key` pressed with no modifier at all. `consume_key(NONE, …)` also matches the key with
/// Shift or Alt held, which a text field must leave to the shortcuts.
pub fn consume_bare(ui: &egui::Ui, key: Key) -> bool {
    ui.input_mut(|i| {
        let at = i.events.iter().position(
            |event| matches!(event, egui::Event::Key { key: k, pressed: true, modifiers, .. } if *k == key && modifiers.is_none()),
        );
        at.map(|at| i.events.remove(at)).is_some()
    })
}

/// Scrolls the focused panel by rows: a line for the arrows, a page for PageUp/PageDown,
/// `rows` bounds Home and End. Returns the new offset when a key moved it, else `jump`.
pub fn keyboard_scroll(
    ui: &egui::Ui,
    id: egui::Id,
    row_h: f32,
    rows: std::ops::Range<usize>,
    jump: Option<f32>,
) -> Option<f32> {
    if !ui.memory(|m| m.has_focus(focus_id())) {
        return jump;
    }
    let mut offset = jump.unwrap_or_else(|| {
        egui::scroll_area::State::load(ui.ctx(), id).map(|s| s.clamped_offset().y).unwrap_or_default()
    });
    let height = ui.available_height();
    let page = (height - row_h).max(row_h);
    let mut moved = false;
    for key in take_keys(ui, &NAV_KEYS[..6]) {
        offset = match key {
            Key::ArrowUp => offset - row_h,
            Key::ArrowDown => offset + row_h,
            Key::PageUp => offset - page,
            Key::PageDown => offset + page,
            Key::Home => rows.start as f32 * row_h,
            _ => (rows.end as f32 * row_h - height).max(rows.start as f32 * row_h),
        };
        offset = offset.max(0.0);
        moved = true;
    }
    if moved { Some(offset) } else { jump }
}

/// The section `delta` away from `current`, clamped to `0..len`. With no current section
/// the first (or, going back, the last) one.
pub fn step_index(current: Option<usize>, len: usize, delta: i32) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match current {
        Some(i) => (i as i32 + delta).clamp(0, len as i32 - 1) as usize,
        None if delta < 0 => len - 1,
        None => 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stepping_clamps_and_starts_at_the_ends() {
        assert_eq!(step_index(None, 0, 1), None);
        assert_eq!(step_index(None, 3, 1), Some(0));
        assert_eq!(step_index(None, 3, -1), Some(2));
        assert_eq!(step_index(Some(2), 3, 1), Some(2));
        assert_eq!(step_index(Some(0), 3, -1), Some(0));
        assert_eq!(step_index(Some(1), 3, 1), Some(2));
    }
}
