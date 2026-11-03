//! What one UI holds for itself, apart from the host: the palette it draws with, the cards it has open,
//! what waits for the user's confirmation, the terminal to focus, and the commands its last draw recorded.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use atomics_core::layout::DiffSplit;
use atomics_core::plan::PlanStatus;

use crate::Command;
use crate::diff_view::{DiffState, ExplorerState};
use crate::theme::{Fonts, Palette};
use crate::touch::Touch;

/// One UI's own state: the desktop keeps one per window, a browser one per page. Fields arrive here from
/// the desktop's client as the draw code that uses them moves into this crate.
pub struct Client {
    /// The colors and fonts to draw with, made by the platform from the host's theme.
    pub palette: Arc<Palette>,
    /// What the user asked for while this UI drew, oldest first. The platform takes them after each
    /// draw and carries them out.
    pub commands: Vec<Command>,
    /// The task cards open on boards, by plan and task id.
    pub expanded_tasks: HashSet<(String, String)>,
    /// A plan waiting for the user to confirm closing it, and the status it would close with.
    pub confirm_plan_close: Option<(String, PlanStatus)>,
    /// Permission requests decided here that the host has not answered yet, by session and request id.
    pub approval_decisions: HashSet<(String, String)>,
    /// Each worktree's diff view: scroll, focus, folds, the file open whole and review comments, by root.
    pub diff_views: HashMap<PathBuf, DiffState>,
    /// Side by side or unified, picked in the diff header; wins over the config until the UI closes.
    pub diff_split: Option<DiffSplit>,
    /// Each project's explorer: its filter, open folders and whether its worktrees show, by project.
    pub explorers: HashMap<PathBuf, ExplorerState>,
    /// The sessions picked to steer together, by id.
    pub selected: HashSet<String>,
    /// A session waiting for the user to confirm closing it, by id.
    pub confirm_close: Option<String>,
    /// The session whose terminal takes the keyboard on the next frame, by id.
    pub focus_terminal: Option<String>,
    /// The ATOMICS icon, once the platform has loaded it.
    pub icon: Option<egui::TextureHandle>,
    /// The name typed for saving the open projects as a workspace.
    pub workspace_input: String,
    /// Set while the platform draws for a finger on a small screen; the desktop leaves it `None`.
    pub touch: Option<Touch>,
}

impl Default for Client {
    /// A client drawing with the default built-in theme until the platform gives it the host's.
    fn default() -> Self {
        let tokens = atomics_core::theme::builtin(atomics_core::theme::DEFAULT).expect("the default theme is built in");
        Self {
            palette: Arc::new(Palette::new(&tokens, Fonts::default())),
            commands: Vec::new(),
            expanded_tasks: HashSet::new(),
            confirm_plan_close: None,
            approval_decisions: HashSet::new(),
            diff_views: HashMap::new(),
            diff_split: None,
            explorers: HashMap::new(),
            selected: HashSet::new(),
            confirm_close: None,
            focus_terminal: None,
            icon: None,
            workspace_input: String::new(),
            touch: None,
        }
    }
}

impl Client {
    /// Records what the user asked for; the platform carries it out after the draw.
    pub fn send(&mut self, command: Command) {
        self.commands.push(command);
    }
}
