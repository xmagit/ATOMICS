//! ATOMICS in a browser: the egui UI of `atomics-ui`, drawn over a remote host reached through a
//! rendezvous broker (docs/decisions/2026-11-02-embedded-atomics.md §4; docs/daemon.md, "The browser
//! client and its embedding").
//!
//! - [`boot`]: the hosting page's boot and tickets, by `postMessage` from its exact origin.
//! - [`connection`]: the Noise channel through the broker (pairing, then sessions).
//! - [`remote`]: the UI's `Backend` over `atomics.v1`, on `atomics_core::mirror`.
//! - [`screens`]: terminals: following screens, the lease and typing, history.
//! - [`driver`]: the frame's life with its host, reconnecting included, as a state machine.
//! - [`views`]: the screens, drawn with atomics-ui's views: the desktop's own frame when the window is
//!   wide, a shared widget a pane on a phone ([`layout`]); what the page shows of its host is its [`desk`];
//!   terminals in a [`term_pane`], touched through [`gesture`]s.
//! - `web` (wasm32 only): the browser glue that runs the driver's outputs.
//!
//! Everything but `web` builds natively, so the rules are tested without a browser.

pub mod boot;
pub mod connection;
pub mod desk;
pub mod device;
pub mod driver;
pub mod gesture;
pub mod layout;
#[cfg(not(target_arch = "wasm32"))]
pub mod manifest;
pub mod page;
pub mod remote;
pub mod screens;
pub mod term_pane;
pub mod views;
#[cfg(target_arch = "wasm32")]
pub mod web;

#[cfg(test)]
mod fake_host;
#[cfg(test)]
mod world;

/// The client is a generic one: nothing in this crate names a hosting product (design §7).
#[cfg(test)]
mod names {
    use std::path::Path;

    fn files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            let name = entry.file_name();
            if path.is_dir() && name != "dist" {
                files(&path, out);
            } else if path.is_file() {
                out.push(path);
            }
        }
    }

    #[test]
    fn no_hosting_product_is_named() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut found = Vec::new();
        files(root, &mut found);
        assert!(found.iter().any(|p| p.ends_with("web/device-store.js")), "the walk reaches the browser files");
        files(&root.join("../atomics-ui/src"), &mut found);
        let needles = [["re", "lay"].concat(), ["atom", "isk"].concat()];
        for path in found {
            let text = std::fs::read_to_string(&path).unwrap_or_default().to_lowercase();
            for needle in &needles {
                assert!(!text.contains(needle), "{} names a hosting product", path.display());
            }
        }
    }
}
