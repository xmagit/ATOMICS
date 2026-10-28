//! UI-independent core of cox: config, git diffs, responsive layout, the hook
//! protocol shared by `cox hook` and the app, and steering message composition.

pub mod config;
pub mod control;
pub mod diff;
pub mod files;
pub mod git;
pub mod hook;
pub mod layout;
pub mod paths;
pub mod plan;
pub mod session;
pub mod steer;
pub mod store;
pub mod workspace;
