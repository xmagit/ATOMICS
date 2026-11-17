use super::*;
use ring::signature::KeyPair;
use serde_json::json;
use std::net::TcpListener;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

fn key() -> ring::signature::Ed25519KeyPair {
    let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
    ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap()
}

fn checksum(bytes: &[u8], name: &str) -> Vec<u8> {
    let digest: String =
        ring::digest::digest(&ring::digest::SHA256, bytes).as_ref().iter().map(|b| format!("{b:02x}")).collect();
    format!("{digest}  {name}\n").into_bytes()
}

struct Server {
    base: String,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    requests: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Server {
    fn new(files: BTreeMap<String, Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let received = requests.clone();
        let thread = std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let Ok((mut socket, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while socket.read_exact(&mut byte).is_ok() {
                    request.push(byte[0]);
                    if request.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&request);
                let path = text.split_whitespace().nth(1).unwrap_or("").trim_start_matches('/');
                received.lock().unwrap().push(path.to_string());
                let (status, body) =
                    files.get(path).map_or(("404 Not Found", &b"missing"[..]), |b| ("200 OK", b.as_slice()));
                let _ =
                    write!(socket, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = socket.write_all(body);
            }
        });
        Self { base, stop, thread: Some(thread), requests }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn signed_asset(name: &str, bytes: &[u8], key: &ring::signature::Ed25519KeyPair) -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([
        (name.into(), bytes.into()),
        (format!("{name}.sha256"), checksum(bytes, name)),
        (format!("{name}.sig"), key.sign(bytes).as_ref().into()),
    ])
}

#[test]
fn size_defaults_leave_four_cores_and_half_memory_for_the_mac() {
    for (cores, gib, expected) in [(18, 48, (14, 24)), (8, 16, (4, 8)), (4, 8, (1, 4)), (2, 1, (1, 1))] {
        let sizes = Sizes::default().resolve(cores, gib * GIB).unwrap();
        assert_eq!((sizes.cpus, sizes.memory_gib, sizes.disk_gib), (expected.0, expected.1, 100));
    }
}

#[test]
fn explicit_sizes_and_invalid_sizes() {
    let options = Sizes { cpus: Some(6), memory_gib: Some(7), disk_gib: Some(200) };
    assert_eq!(options.resolve(18, 48 * GIB).unwrap(), BuildInfo { cpus: 6, memory_gib: 7, disk_gib: 200 });
    assert!(Sizes { cpus: Some(0), ..Sizes::default() }.resolve(8, 16 * GIB).is_err());
    assert!(Sizes { memory_gib: Some(0), ..Sizes::default() }.resolve(8, 16 * GIB).is_err());
    assert!(Sizes { disk_gib: Some(99), ..Sizes::default() }.resolve(8, 16 * GIB).is_err());
}

fn template() -> Template {
    let file = |contents: &str, executable: bool| json!({"contents": contents, "executable": executable});
    let bytes = serde_json::to_vec(&json!({
        "version": 1, "releaseVersion": "1.2.3", "image": "atomics-vm-1.2.3-aarch64-linux.raw.zst",
        "imageSha256": "a".repeat(64), "imageBytes": IMAGE_BYTES,
        "files": {
            "install.sh": file("#!/bin/sh\nmkdir -p '/Users/@ATOMICS_MAC_USER@/repos'\n", true),
            "bin/atomics-vm-host-forward": file("#!/bin/sh\necho '/Users/@ATOMICS_MAC_USER@'\n", true),
            "lima.yaml": file(r#"{"param":{"ATOMICS_MAC_USER":"@ATOMICS_MAC_USER@"},"cpus":4,"memory":"8GiB","disk":"100GiB","mounts":[{"location":"/Users/@ATOMICS_MAC_USER@/repos"}]}"#, false),
            "atomics-vm.json": file(r#"{"version":1,"instance":"atomics","user":"atomics","home":"/home/atomics","macUser":"@ATOMICS_MAC_USER@","installDir":"/Users/@ATOMICS_MAC_USER@/.local/share/atomics-vm","host":{"port":47800}}"#, false),
            "config.d.toml": file("[client]\nconnect = \"vm\"\n", false),
            "launchd/forward.plist": file("<string>/Users/@ATOMICS_MAC_USER@</string>", false)
        }
    })).unwrap();
    Template::read(&bytes, "1.2.3", Flavor::Standard).unwrap()
}

#[test]
fn hardened_release_manifest_accepts_its_own_image_only() {
    let manifest = serde_json::json!({
        "version": 1, "releaseVersion": "1.2.3", "flavor": "hardened",
        "image": "atomics-vm-hardened-1.2.3-aarch64-linux.raw.zst",
        "imageSha256": "0".repeat(64), "imageBytes": IMAGE_BYTES, "files": {}
    });
    assert!(Template::read(&serde_json::to_vec(&manifest).unwrap(), "1.2.3", Flavor::Hardened).is_ok());
    assert!(Template::read(&serde_json::to_vec(&manifest).unwrap(), "1.2.3", Flavor::Standard).is_err());
    let mut swapped = manifest;
    swapped["image"] = "atomics-vm-1.2.3-aarch64-linux.raw.zst".into();
    assert!(Template::read(&serde_json::to_vec(&swapped).unwrap(), "1.2.3", Flavor::Hardened).is_err());
}

#[test]
fn hardened_download_never_requests_standard_assets_when_missing_or_untrusted() {
    let trusted = key();
    let other = key();
    for available in [false, true] {
        let mut files = signed_asset("atomics-vm-1.2.3-aarch64-linux.bundle.json", b"standard", &trusted);
        if available {
            files.extend(signed_asset("atomics-vm-hardened-1.2.3-aarch64-linux.bundle.json", b"untrusted", &other));
        }
        let server = Server::new(files);
        let dir = tempfile::tempdir().unwrap();
        assert!(
            fetch(
                &server.base,
                "1.2.3",
                Flavor::Hardened,
                dir.path(),
                "alice",
                &Sizes::default().resolve(8, 16 * GIB).unwrap(),
                &[trusted.public_key().as_ref().try_into().unwrap()],
                &|_| {}
            )
            .is_err()
        );
        assert!(server.requests.lock().unwrap().iter().all(|path| path.starts_with("atomics-vm-hardened-")));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "no unpacking or bundle scripts");
    }
}

#[test]
fn fills_generated_bundle_files_and_sizes_consistently() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("bundle");
    let sizes = BuildInfo { cpus: 12, memory_gib: 24, disk_gib: 200 };
    template().fill(&bundle, "alice", &sizes).unwrap();
    let info = BundleInfo::read(&bundle).unwrap();
    assert_eq!(info.mac_user, "alice");
    assert_eq!(info.build, Some(sizes));
    let lima: serde_json::Value = serde_json::from_slice(&std::fs::read(bundle.join("lima.yaml")).unwrap()).unwrap();
    assert_eq!(lima["param"]["ATOMICS_MAC_USER"], "alice");
    assert_eq!(lima["cpus"], 12);
    assert_eq!(lima["memory"], "24GiB");
    assert_eq!(lima["disk"], "200GiB");
    assert_eq!(lima["mounts"][0]["location"], "/Users/alice/repos");
    assert_eq!(std::fs::read_to_string(bundle.join("config.d.toml")).unwrap(), "[client]\nconnect = \"vm\"\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(bundle.join("install.sh")).unwrap().permissions().mode() & 0o777, 0o755);
    }
}

#[test]
fn refuses_template_traversal_unknown_tokens_and_unsafe_accounts() {
    let dir = tempfile::tempdir().unwrap();
    let sizes = Sizes::default().resolve(8, 16 * GIB).unwrap();
    for user in ["", "../alice", "alice'", "a$b", "a\nb", "-alice"] {
        assert!(template().fill(&dir.path().join("bundle"), user, &sizes).is_err());
    }
    for path in ["../outside", "/outside", "bin/../../outside"] {
        let mut template = template();
        template.files.insert(path.into(), TemplateFile { contents: "oops".into(), executable: false });
        assert!(template.fill(&dir.path().join("bundle"), "alice", &sizes).is_err());
        assert!(!dir.path().join("bundle").exists());
    }
    let mut template = template();
    template.files.get_mut("install.sh").unwrap().contents = "@ATOMICS_UNKNOWN@".into();
    assert!(template.fill(&dir.path().join("bundle"), "alice", &sizes).is_err());
}

#[test]
fn download_accepts_either_release_key_and_refuses_bad_checksum_or_signature() {
    let ci = key();
    let offline = key();
    let unknown = key();
    let keys = [ci.public_key().as_ref().try_into().unwrap(), offline.public_key().as_ref().try_into().unwrap()];
    let name = "image.raw.zst";
    let dir = tempfile::tempdir().unwrap();
    for signer in [&ci, &offline] {
        let server = Server::new(signed_asset(name, b"compressed image", signer));
        let mut file =
            download_asset(&ureq::Agent::new_with_defaults(), &server.base, name, dir.path(), 1024, &keys, &|_| {})
                .unwrap();
        let mut actual = Vec::new();
        file.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, b"compressed image");
    }
    for failure in ["checksum", "signature", "key", "missing", "limit", "renamed"] {
        let mut files = signed_asset(name, b"compressed image", if failure == "key" { &unknown } else { &ci });
        match failure {
            "checksum" => {
                files.insert(format!("{name}.sha256"), checksum(b"other", name));
            }
            "signature" => {
                files.insert(format!("{name}.sig"), vec![0; 64]);
            }
            "missing" => {
                files.remove(&format!("{name}.sig"));
            }
            "renamed" => {
                files.insert(format!("{name}.sha256"), checksum(b"compressed image", "other"));
            }
            _ => {}
        }
        let server = Server::new(files);
        let result = download_asset(
            &ureq::Agent::new_with_defaults(),
            &server.base,
            name,
            dir.path(),
            if failure == "limit" { 2 } else { 1024 },
            &keys,
            &|_| {},
        );
        assert!(result.is_err(), "{failure}");
    }
}

#[test]
fn signed_manifest_refuses_wrong_version_or_another_versions_image() {
    let valid = json!({"version":1,"releaseVersion":"1.2.3","image":"atomics-vm-1.2.3-aarch64-linux.raw.zst","imageSha256":"a".repeat(64),"imageBytes":IMAGE_BYTES,"files":{}});
    assert!(Template::read(&serde_json::to_vec(&valid).unwrap(), "1.2.4", Flavor::Standard).is_err());
    for (field, value) in [
        ("image", json!("atomics-vm-1.2.2-aarch64-linux.raw.zst")),
        ("imageBytes", json!(0)),
        ("imageSha256", json!("bad")),
        ("version", json!(2)),
    ] {
        let mut bad = valid.clone();
        bad[field] = value;
        assert!(Template::read(&serde_json::to_vec(&bad).unwrap(), "1.2.3", Flavor::Standard).is_err());
    }
}

#[test]
fn version_binding_rejects_a_validly_signed_image_before_unpacking() {
    let key = key();
    let bytes = b"an older image, correctly signed";
    let name = "atomics-vm-1.2.3-aarch64-linux.raw.zst";
    let manifest = serde_json::to_vec(&json!({"version":1,"releaseVersion":"1.2.3","image":name,"imageSha256":"a".repeat(64),"imageBytes":IMAGE_BYTES,"files":{}})).unwrap();
    let mut files = signed_asset(name, bytes, &key);
    files.extend(signed_asset("atomics-vm-1.2.3-aarch64-linux.bundle.json", &manifest, &key));
    let server = Server::new(files);
    let dir = tempfile::tempdir().unwrap();
    let result = fetch(
        &server.base,
        "1.2.3",
        Flavor::Standard,
        dir.path(),
        "alice",
        &Sizes::default().resolve(8, 16 * GIB).unwrap(),
        &[key.public_key().as_ref().try_into().unwrap()],
        &|_| {},
    );
    assert!(result.unwrap_err().to_string().contains("does not match"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "nothing unpacked before verification");
}

#[test]
fn sparse_decompression_preserves_bytes_and_trailing_holes() {
    let dir = tempfile::tempdir().unwrap();
    let mut bytes = vec![0; 8 * 1024 * 1024];
    bytes[..4].copy_from_slice(b"disk");
    let compressed = zstd::stream::encode_all(bytes.as_slice(), 1).unwrap();
    let source = dir.path().join("image.zst");
    std::fs::write(&source, &compressed).unwrap();
    let output = dir.path().join("image.raw");
    decompress(File::open(&source).unwrap(), &output, bytes.len() as u64, &|_| {}).unwrap();
    assert_eq!(std::fs::read(&output).unwrap(), bytes);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert!(std::fs::metadata(&output).unwrap().blocks() * 512 < bytes.len() as u64 / 2);
    }
    assert!(decompress(File::open(&source).unwrap(), &dir.path().join("small.raw"), 8, &|_| {}).is_err());
    assert!(
        decompress(File::open(&source).unwrap(), &dir.path().join("large.raw"), bytes.len() as u64 + 1, &|_| {})
            .is_err()
    );
    std::fs::write(&source, &compressed[..compressed.len() / 2]).unwrap();
    assert!(
        decompress(File::open(source).unwrap(), &dir.path().join("broken.raw"), bytes.len() as u64, &|_| {}).is_err()
    );
}
