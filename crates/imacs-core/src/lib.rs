//! UI-independent core of IMACS: config, git diffs, responsive layout, the hook
//! protocol shared by `imacs hook` and the app, steering message composition, and voice transcription.
//!
//! Disable the default `native` feature for browser clients. Process, filesystem,
//! home-directory, clock and voice helpers require `native`; shared data and pure logic do not.

#[cfg(all(test, feature = "native"))]
#[path = "../../../tests/support.rs"]
mod test_support;

#[cfg(all(test, feature = "native"))]
mod platform_tests;

pub mod approvals;
#[cfg(feature = "native")]
pub mod cleanup;
pub mod config;
pub mod control;
pub mod diff;
pub mod doctor;
pub mod files;
#[cfg(feature = "native")]
pub mod git;
pub mod governor;
pub mod heartbeat;
pub mod hook;
pub mod layout;
#[cfg(feature = "native")]
pub mod paths;
pub mod plan;
pub mod session;
pub mod steer;
pub mod store;
pub mod theme;
pub mod tickets;
pub mod usage;
#[cfg(feature = "native")]
pub mod voice;
pub mod workspace;
