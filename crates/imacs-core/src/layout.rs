//! Responsive layout: as the window grows, more agents get their own tile and the
//! diff moves beside them; as it shrinks, tiles collapse into tabs and the diff
//! stacks above the active agent, and finally takes the whole main area.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutConfig {
    pub min_tile_width: f32,
    pub min_tile_height: f32,
    pub sidebar_width: f32,
    pub sidebar_min_window: f32,
    pub diff_min_width: f32,
    pub diff_fraction: f32,
    /// Widest the diff gets beside the agents; the rest goes to more agent columns. 0 = no cap.
    pub diff_max_width: f32,
    pub diff_min_height: f32,
    pub diff_split: DiffSplit,
    /// Width set by dragging the divider; wins over fraction and cap.
    #[serde(skip)]
    pub diff_width: Option<f32>,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            min_tile_width: 520.0,
            min_tile_height: 280.0,
            sidebar_width: 250.0,
            sidebar_min_window: 1100.0,
            diff_min_width: 560.0,
            diff_fraction: 0.55,
            diff_max_width: 1400.0,
            diff_min_height: 260.0,
            diff_split: DiffSplit::Auto,
            diff_width: None,
        }
    }
}

/// When the diff shows the old and the new version side by side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffSplit {
    /// When the diff area is at least `SPLIT_MIN_WIDTH` wide.
    #[default]
    Auto,
    Always,
    Never,
}

pub const SPLIT_MIN_WIDTH: f32 = 1100.0;

impl DiffSplit {
    pub fn split(self, diff_width: f32) -> bool {
        match self {
            DiffSplit::Auto => diff_width >= SPLIT_MIN_WIDTH,
            DiffSplit::Always => true,
            DiffSplit::Never => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DiffPlacement {
    Hidden,
    /// Diff on the right, agents on the left.
    Right {
        width: f32,
    },
    /// Diff above, the active agent below.
    Top {
        height: f32,
    },
    /// Diff only; the agents are reachable through the tab strip.
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub sidebar: bool,
    pub dock_width: f32,
    pub diff: DiffPlacement,
    pub columns: usize,
    pub rows: usize,
    /// How many sessions get a tile. The rest are tabs.
    pub tiled: usize,
    pub tabs: bool,
}

/// Width/height ratio a terminal tile looks best at.
const TARGET_TILE_ASPECT: f32 = 1.7;

pub const MIN_DOCK_WIDTH: f32 = 520.0;

pub fn compute(
    width: f32,
    height: f32,
    sessions: usize,
    diff_open: bool,
    dock_width: f32,
    cfg: &LayoutConfig,
) -> Layout {
    let mut sidebar = width >= cfg.sidebar_min_window;
    let available = |sidebar: bool| if sidebar { width - cfg.sidebar_width } else { width };
    let remaining = cfg.min_tile_width + if diff_open { cfg.diff_min_width } else { 0.0 };
    let dock_requested = dock_width.is_finite() && dock_width > 0.0;
    if dock_requested
        && sidebar
        && available(true) < MIN_DOCK_WIDTH + remaining
        && available(false) >= MIN_DOCK_WIDTH + remaining
    {
        sidebar = false;
    }
    let dock_width = if dock_requested && available(sidebar) >= MIN_DOCK_WIDTH + remaining {
        dock_width.max(MIN_DOCK_WIDTH).min(available(sidebar) - remaining)
    } else {
        0.0
    };
    let main_width = |sidebar: bool| available(sidebar) - dock_width;

    let mut placed = place_diff(main_width(sidebar), height, diff_open, cfg);
    // Reviewing next to the agent beats seeing the session list: drop the sidebar
    // when that is what it takes to keep the diff beside the agents.
    if sidebar && diff_open && !matches!(placed.0, DiffPlacement::Right { .. }) {
        let without = place_diff(main_width(false), height, diff_open, cfg);
        if matches!(without.0, DiffPlacement::Right { .. }) {
            sidebar = false;
            placed = without;
        }
    }
    let (diff, term_width, term_height) = placed;

    if sessions == 0 || diff == DiffPlacement::Full {
        return Layout { sidebar, dock_width, diff, columns: 0, rows: 0, tiled: 0, tabs: sessions > 0 };
    }

    // A diff stacked on top leaves room for exactly one agent below it.
    let (max_cols, max_rows) = match diff {
        DiffPlacement::Top { .. } => (1, 1),
        _ => (
            ((term_width / cfg.min_tile_width).floor() as usize).max(1),
            ((term_height / cfg.min_tile_height).floor() as usize).max(1),
        ),
    };
    let tiled = sessions.min(max_cols * max_rows);
    let (columns, rows) = best_grid(tiled, max_cols, max_rows, term_width, term_height);
    Layout { sidebar, dock_width, diff, columns, rows, tiled, tabs: sessions > tiled }
}

/// Returns where the diff goes and the space left for agent tiles.
fn place_diff(main_width: f32, height: f32, diff_open: bool, cfg: &LayoutConfig) -> (DiffPlacement, f32, f32) {
    if !diff_open {
        (DiffPlacement::Hidden, main_width, height)
    } else if main_width - cfg.diff_min_width >= cfg.min_tile_width {
        let mut diff_width = main_width * cfg.diff_fraction;
        if cfg.diff_max_width > 0.0 {
            diff_width = diff_width.min(cfg.diff_max_width);
        }
        if let Some(dragged) = cfg.diff_width {
            diff_width = dragged;
        }
        let diff_width = diff_width.max(cfg.diff_min_width).min(main_width - cfg.min_tile_width);
        (DiffPlacement::Right { width: diff_width }, main_width - diff_width, height)
    } else if height - cfg.diff_min_height >= cfg.min_tile_height {
        let diff_height = (height * cfg.diff_fraction).max(cfg.diff_min_height).min(height - cfg.min_tile_height);
        (DiffPlacement::Top { height: diff_height }, main_width, height - diff_height)
    } else {
        (DiffPlacement::Full, 0.0, 0.0)
    }
}

/// Picks the grid whose tiles are closest to a terminal-friendly aspect ratio,
/// with a small penalty for empty cells.
fn best_grid(tiles: usize, max_cols: usize, max_rows: usize, width: f32, height: f32) -> (usize, usize) {
    let mut best = (1, tiles.max(1));
    let mut best_score = f32::MAX;
    for cols in 1..=max_cols.min(tiles) {
        let rows = tiles.div_ceil(cols);
        if rows > max_rows {
            continue;
        }
        let aspect = (width / cols as f32) / (height / rows as f32);
        let empty = (cols * rows - tiles) as f32;
        let score = (aspect.ln() - TARGET_TILE_ASPECT.ln()).abs() + empty * 0.35;
        if score < best_score {
            best_score = score;
            best = (cols, rows);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> LayoutConfig {
        LayoutConfig::default()
    }

    #[test]
    fn ultrawide_tiles_everything_beside_the_diff() {
        let layout = compute(3440.0, 1400.0, 4, true, 0.0, &cfg());
        assert!(layout.sidebar);
        let DiffPlacement::Right { width } = layout.diff else { panic!("{layout:?}") };
        assert!(width >= 560.0);
        assert_eq!(layout.tiled, 4);
        assert!(!layout.tabs);
        assert!(layout.columns * layout.rows >= 4);
    }

    #[test]
    fn super_ultrawide_caps_the_diff_and_gives_agents_the_width() {
        let layout = compute(5120.0, 1360.0, 10, true, 0.0, &cfg());
        assert_eq!(layout.diff, DiffPlacement::Right { width: 1400.0 });
        assert_eq!((layout.tiled, layout.columns, layout.rows), (10, 5, 2));
        assert!(!layout.tabs);
    }

    #[test]
    fn dragged_width_wins_but_leaves_room_for_an_agent() {
        let dragged = LayoutConfig { diff_width: Some(2600.0), ..cfg() };
        let layout = compute(5120.0, 1360.0, 4, true, 0.0, &dragged);
        assert_eq!(layout.diff, DiffPlacement::Right { width: 2600.0 });
        let too_wide = LayoutConfig { diff_width: Some(9000.0), ..cfg() };
        let DiffPlacement::Right { width } = compute(3000.0, 1200.0, 2, true, 0.0, &too_wide).diff else { panic!() };
        assert_eq!(width, 3000.0 - 250.0 - 520.0);
    }

    #[test]
    fn laptop_keeps_diff_beside_and_overflows_into_tabs() {
        let layout = compute(1512.0, 900.0, 5, true, 0.0, &cfg());
        assert!(layout.sidebar);
        assert!(matches!(layout.diff, DiffPlacement::Right { .. }));
        assert!(layout.tiled >= 1 && layout.tiled < 5);
        assert!(layout.tabs);
    }

    #[test]
    fn narrow_window_stacks_diff_over_one_agent() {
        let layout = compute(900.0, 1000.0, 3, true, 0.0, &cfg());
        assert!(!layout.sidebar);
        assert!(matches!(layout.diff, DiffPlacement::Top { .. }));
        assert_eq!((layout.tiled, layout.columns, layout.rows), (1, 1, 1));
        assert!(layout.tabs);
    }

    #[test]
    fn tiny_window_shows_only_the_diff_with_tabs() {
        let layout = compute(700.0, 450.0, 2, true, 0.0, &cfg());
        assert_eq!(layout.diff, DiffPlacement::Full);
        assert_eq!(layout.tiled, 0);
        assert!(layout.tabs);
    }

    #[test]
    fn closed_diff_gives_agents_the_whole_area() {
        let layout = compute(2000.0, 1200.0, 6, false, 0.0, &cfg());
        assert_eq!(layout.diff, DiffPlacement::Hidden);
        assert_eq!(layout.tiled, 6);
        assert_eq!(layout.columns * layout.rows, 6);
    }

    #[test]
    fn dock_reduces_tile_space_and_preserves_diff() {
        let floating = compute(5120.0, 1360.0, 20, true, 0.0, &cfg());
        let docked = compute(5120.0, 1360.0, 20, true, 1655.8, &cfg());
        assert_eq!(docked.dock_width, 1655.8);
        assert_eq!(docked.diff, floating.diff);
        assert!(docked.tiled < floating.tiled);
        assert!(docked.tabs);
    }

    #[test]
    fn narrow_windows_hide_dock_before_stacking_diff() {
        let laptop = compute(1600.0, 1000.0, 4, true, 520.0, &cfg());
        assert!(!laptop.sidebar);
        assert_eq!(laptop.dock_width, 520.0);
        assert!(matches!(laptop.diff, DiffPlacement::Right { .. }));
        let narrow = compute(1512.0, 900.0, 4, true, 520.0, &cfg());
        assert_eq!(narrow.dock_width, 0.0);
        assert!(matches!(narrow.diff, DiffPlacement::Right { .. }));
        let tiny = compute(700.0, 450.0, 4, true, 520.0, &cfg());
        assert_eq!(tiny.dock_width, 0.0);
        assert_eq!(tiny.diff, DiffPlacement::Full);
    }

    #[test]
    fn dock_width_clamps_and_closed_diff_reclaims_space() {
        let clamped = compute(2000.0, 1000.0, 4, true, 9000.0, &cfg());
        assert_eq!(clamped.dock_width, 670.0);
        assert_eq!(clamped.diff, DiffPlacement::Right { width: 560.0 });
        let closed = compute(1200.0, 1000.0, 4, false, 520.0, &cfg());
        assert_eq!(closed.dock_width, 520.0);
        assert_eq!(closed.diff, DiffPlacement::Hidden);
        assert_eq!(compute(900.0, 1000.0, 4, false, 520.0, &cfg()).dock_width, 0.0);
    }

    #[test]
    fn auto_split_needs_a_wide_diff() {
        assert!(!DiffSplit::Auto.split(1099.0));
        assert!(DiffSplit::Auto.split(1100.0));
        assert!(DiffSplit::Always.split(400.0));
        assert!(!DiffSplit::Never.split(3000.0));
    }

    #[test]
    fn sidebar_yields_to_keep_the_diff_beside_the_agent() {
        let layout = compute(1300.0, 800.0, 1, true, 0.0, &cfg());
        assert!(!layout.sidebar);
        assert!(matches!(layout.diff, DiffPlacement::Right { .. }));
        assert_eq!(layout.tiled, 1);
        assert!(!layout.tabs);
    }
}
