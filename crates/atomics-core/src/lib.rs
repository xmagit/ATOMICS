//! UI-independent core of ATOMICS: config, git diffs, responsive layout, the hook
//! protocol shared by `atomics hook` and the app, steering message composition, and voice transcription.
//!
//! Disable the default `native` feature for browser clients. Process, filesystem,
//! home-directory, clock and voice helpers require `native`; shared data and pure logic do not.

#[cfg(all(test, feature = "native"))]
#[path = "../../../tests/support.rs"]
mod test_support;

#[cfg(all(test, feature = "native"))]
mod platform_tests;

#[cfg(feature = "native")]
pub mod advert;
pub mod approvals;
pub mod cache;
#[cfg(feature = "native")]
pub mod cleanup;
pub mod config;
pub mod control;
pub mod diff;
pub mod diff_data;
pub mod doctor;
pub mod files;
pub mod folder;
#[cfg(feature = "native")]
pub mod git;
pub mod github;
pub mod governor;
pub mod heartbeat;
pub mod hook;
pub mod identity;
pub mod isolation;
pub mod layout;
pub mod lease;
pub mod machine;
pub mod mcp_import;
pub mod mirror;
pub mod noise;
pub mod ops;
#[cfg(feature = "native")]
pub mod paths;
pub mod plan;
pub mod preview;
pub mod projection;
pub mod remote;
pub mod repo_workflow;
pub mod screen;
pub mod secrets;
pub mod selfconfig;
pub mod session;
pub mod stash;
pub mod steer;
pub mod store;
pub mod theme;
pub mod ticket_ui;
pub mod tickets;
pub mod trace;
pub mod trace_feed;
pub mod tunnel;
pub mod usage;
#[cfg(feature = "native")]
pub mod voice;
pub mod wire;
pub mod workflow;
pub mod workspace;
