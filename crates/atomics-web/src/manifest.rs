//! The bundle's manifest: the ATOMICS version, the
//! protocol it speaks, the wasm's SHA-256 and the commit it was built from, plus every file's SHA-256, so
//! a hosting product can pin the bundle and check what it serves.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use serde::Serialize;
use sha2::{Digest, Sha256};

/// The manifest's name in the bundle.
pub const FILE: &str = "manifest.json";

#[derive(Debug, Serialize)]
pub struct Manifest {
    pub atomics_version: &'static str,
    pub protocol_major: u16,
    /// Minor versions are additive: the highest this client speaks.
    pub protocol_minor_max: u16,
    pub wasm_sha256: String,
    pub built_from: String,
    /// Every file of the bundle but the manifest, by its path in the bundle.
    pub files: BTreeMap<String, String>,
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn walk(dir: &Path, prefix: &str, files: &mut BTreeMap<String, String>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        if entry.file_type()?.is_dir() {
            walk(&entry.path(), &path, files)?;
        } else if path != FILE {
            files.insert(path, sha256(&fs::read(entry.path())?));
        }
    }
    Ok(())
}

/// The manifest of the bundle in `dist`, which must hold exactly one `.wasm`.
pub fn read(dist: &Path, built_from: &str) -> io::Result<Manifest> {
    let mut files = BTreeMap::new();
    walk(dist, "", &mut files)?;
    let wasm: Vec<&String> = files.iter().filter(|(path, _)| path.ends_with(".wasm")).map(|(_, sha)| sha).collect();
    let [wasm] = wasm.as_slice() else {
        return Err(io::Error::other(format!("{} holds {} .wasm files, not one", dist.display(), wasm.len())));
    };
    Ok(Manifest {
        atomics_version: env!("CARGO_PKG_VERSION"),
        protocol_major: atomics_core::wire::VERSION.major,
        protocol_minor_max: atomics_core::wire::VERSION.minor,
        wasm_sha256: (*wasm).clone(),
        built_from: built_from.to_string(),
        files,
    })
}

/// Write `dist`'s manifest into it, and return its text.
pub fn write(dist: &Path, built_from: &str) -> io::Result<String> {
    let manifest = read(dist, built_from)?;
    let text = serde_json::to_string_pretty(&manifest).map_err(io::Error::other)? + "\n";
    fs::write(dist.join(FILE), &text)?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_pins_the_wasm_and_every_file() {
        let dist = tempfile::tempdir().unwrap();
        fs::write(dist.path().join("index.html"), "<!doctype html>").unwrap();
        fs::create_dir(dist.path().join("snippets")).unwrap();
        fs::write(dist.path().join("snippets/store.js"), "export {}").unwrap();
        fs::write(dist.path().join("atomics-web-0123_bg.wasm"), b"\0asm").unwrap();
        let text = write(dist.path(), "abc123").unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["protocol_major"], 1);
        assert_eq!(value["built_from"], "abc123");
        assert_eq!(value["wasm_sha256"], sha256(b"\0asm"));
        let files: Vec<&str> = value["files"].as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(files, ["atomics-web-0123_bg.wasm", "index.html", "snippets/store.js"]);
        // Written again, the manifest leaves itself out.
        assert_eq!(write(dist.path(), "abc123").unwrap(), text);

        fs::write(dist.path().join("second.wasm"), b"x").unwrap();
        assert!(read(dist.path(), "abc123").is_err(), "one wasm, not two");
    }
}
