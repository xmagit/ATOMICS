//! The ATOMICS UI's view of a host. The UI reads what it draws from a [`Backend`] and never from the
//! host's model, so the same UI can run over the desktop's in-process model, a loopback client or a
//! browser connection (docs/decisions/2026-11-02-embedded-atomics-extraction.md). The drawing code that
//! reads nothing from the host's model lives here: the diff view, the keyboard navigation it shares with
//! the Tickets panel, the ticket markdown reader, chords, the top bar's layout helpers and the terminal
//! painter, which draws the screens `atomics.v1` streams (the desktop paints through it too). So do the
//! views that draw from a [`Backend`] into a [`Client`]: the plan board, the approval banner, a worktree's
//! diff panel, the sidebar's sessions and explorer, the top bar's navigation, brand, notices, buttons and
//! Project menu, the fleet's tiles and the chief (its pill, its docked or floating window and the steer
//! bar), and the window's frame that lays them out ([`shell`]), with hooks for the items only a host has.
//! The platform draws the terminals in the tiles and in the chief's window through a
//! [`terminal_surface::TerminalSurface`]. They record what the user asks for as [`Command`]s, which the
//! platform carries out after the draw: a host's through [`Backend::send`], a view command on the UI
//! itself; they draw no action [`Backend::available`] says the platform cannot carry out. A platform
//! drawing for a finger on a small screen sets [`Client::touch`], and the views fit a phone ([`touch`]).
//!
//! This crate must stay free of desktop-only dependencies; `tests/dependencies.rs` enforces it.

pub mod approvals;
mod backend;
pub mod board;
pub mod chief;
mod client;
mod command;
pub mod diff_panel;
pub mod diff_view;
pub mod fleet;
pub mod keys;
pub mod machine;
pub mod management;
pub mod markdown;
pub mod nav;
pub mod panel_nav;
pub mod resources;
pub mod shell;
pub mod sidebar;
pub mod signin;
pub mod status;
pub mod switcher;
pub mod tabs;
pub mod terminal;
pub mod terminal_surface;
pub mod text;
pub mod theme;
pub mod tickets;
pub mod topbar;
pub mod touch;
pub mod trace_lane;
pub mod update;
pub mod window_keys;

pub use backend::{
    ApprovalRow, Backend, BoardView, ChiefView, CompletedTile, ConnectButton, DiffHeading, DiffSnapshot,
    FolderSnapshot, Heartbeat, Notices, PanelMode, PlanRow, PlanState, ProjectMenu, RecentSession, SandboxLoad,
    SessionRow, TaskLink, TicketsPanel, TileView, UiConfig, ViewHints, WorktreeRow,
};
pub use client::Client;
pub use command::{Command, Remote};

pub mod editor;
pub mod pull_request;
