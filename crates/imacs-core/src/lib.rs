//! UI-independent core of IMACS: config, git diffs, responsive layout, the hook
//! protocol shared by `imacs hook` and the app, steering message composition, and voice transcription.

#[cfg(test)]
#[path = "../../../tests/support.rs"]
mod test_support;

pub mod approvals;
pub mod config;
pub mod control;
pub mod diff;
pub mod files;
pub mod git;
pub mod governor;
pub mod hook;
pub mod layout;
pub mod paths;
pub mod plan;
pub mod session;
pub mod steer;
pub mod store;
pub mod voice;
pub mod workspace;
