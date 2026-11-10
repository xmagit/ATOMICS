//! The whole way on Linux: a release installed by `scripts/install.sh`, updated by the real program from a local feed
//! with a throwaway key, and rolled back, with what the person has checked byte for byte after each step. Only a debug
//! build can be pointed at another feed and key (`update.rs`); a release build holds no such path, which CI checks.
#![cfg(target_os = "linux")]

#[path = "../../../tests/support.rs"]
mod test_support;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;
use ring::signature::KeyPair;
use test_support::{CHILD_TIMEOUT, TempDir, TestChild, scaled_timeout};

const TARGET: &str = env!("ATOMICS_TARGET");
// The install script's supported Linux release target is a fixture, independent of
// the host running these tests. The updater below still selects the real host target.
const INSTALL_TARGET: &str = "x86_64-unknown-linux-gnu";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const NEW_VERSION: &str = "99.0.0";

/// Made here and gone with the test: no key, real or placeholder, is the trust anchor of anything else.
struct ThrowawayKey(ring::signature::Ed25519KeyPair);

impl ThrowawayKey {
    fn new() -> Self {
        let rng = ring::rand::SystemRandom::new();
        let document = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        ThrowawayKey(ring::signature::Ed25519KeyPair::from_pkcs8(document.as_ref()).unwrap())
    }

    fn sign(&self, bytes: &[u8]) -> Vec<u8> {
        self.0.sign(bytes).as_ref().to_vec()
    }

    fn pem(&self) -> String {
        const PREFIX: [u8; 12] = [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];
        let der = [PREFIX.as_slice(), self.0.public_key().as_ref()].concat();
        format!(
            "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
            base64::engine::general_purpose::STANDARD.encode(der)
        )
    }
}

fn sha256_line(bytes: &[u8], name: &str) -> Vec<u8> {
    let digest: String =
        ring::digest::digest(&ring::digest::SHA256, bytes).as_ref().iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{digest}  {name}\n").into_bytes()
}

fn program(path: &Path, text: &str) {
    test_support::write_program(path, text);
}

/// A tar.gz holding this one program, as a release's does.
fn archive_of(dir: &Path, name: &str, source: &Path) -> Vec<u8> {
    let tree = dir.join(format!("tree-{name}"));
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::copy(source, tree.join("atomics")).unwrap();
    let out = dir.join(format!("{name}.tar.gz"));
    let script = format!("tar -cf - -C '{}' atomics | gzip -1 > '{}'", tree.display(), out.display());
    let packed = test_support::output(Command::new("sh").args(["-c", &script]));
    assert!(packed.status.success(), "{packed:?}");
    std::fs::remove_dir_all(&tree).unwrap();
    std::fs::read(out).unwrap()
}

/// An empty home, a login shell and tools of its own, and what `curl` answers: files named after the addresses.
struct Machine {
    dir: TempDir,
    key: ThrowawayKey,
}

impl Machine {
    fn new() -> Machine {
        let dir = test_support::tempdir().unwrap();
        for folder in ["home", "stubs", "tools", "feed", "runtime"] {
            std::fs::create_dir_all(dir.path().join(folder)).unwrap();
        }
        test_support::link_tools(
            &dir.path().join("tools"),
            &[
                "sh",
                "bash",
                "cat",
                "cp",
                "mv",
                "rm",
                "chmod",
                "mkdir",
                "mktemp",
                "tar",
                "gzip",
                "sed",
                "head",
                "cut",
                "grep",
                "basename",
                "dirname",
                "sha256sum",
                "openssl",
                "git",
                "env",
                "ps",
                "sleep",
                "stty",
                "readlink",
                "ln",
                "id",
                "touch",
                "nice",
            ],
        );
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path().join("runtime"), std::fs::Permissions::from_mode(0o700)).unwrap();
        // Answers from the feed folder, and with curl's failure where there is nothing at that address.
        program(
            &dir.path().join("stubs/curl"),
            "#!/bin/sh\nout=''; url=''\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n    -o) out=$2; shift 2 ;;\n    --proto|--retry) shift 2 ;;\n    -*) shift ;;\n    *) url=$1; shift ;;\n  esac\ndone\nname=$(printf '%s' \"$url\" | sed 's#^https://##; s#/#_#g')\n[ -f \"$FEED/$name\" ] || exit 22\ncp \"$FEED/$name\" \"$out\"\n",
        );
        program(&dir.path().join("stubs/shell"), "#!/bin/sh\nprintf '__ATOMICS__%s__ATOMICS__' \"$PATH\"\n");
        Self::installer_platform(dir.path());
        test_support::copy_program(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/keychain"),
            dir.path().join("stubs/secret-tool"),
        );
        let machine = Machine { dir, key: ThrowawayKey::new() };
        machine.publish_key();
        machine
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn installer_platform(dir: &Path) {
        program(&dir.join("stubs/uname"), "#!/bin/sh\n[ \"$1\" = -s ] && echo Linux || echo x86_64\n");
    }

    fn home(&self) -> PathBuf {
        self.path("home")
    }

    fn feed(&self, url: &str, bytes: impl AsRef<[u8]>) {
        std::fs::write(self.path("feed").join(url.replace('/', "_")), bytes).unwrap();
    }

    fn publish_key(&self) {
        self.feed("raw.githubusercontent.com/xmagit/ATOMICS/main/assets/release-signing-key.pub", self.key.pem());
    }

    /// A release at the addresses `install.sh` reads.
    fn publish(&self, version: &str, archive: &[u8]) {
        self.feed(
            "api.github.com/repos/xmagit/ATOMICS/releases/latest",
            format!("{{\n  \"tag_name\": \"v{version}\",\n  \"name\": \"v{version}\"\n}}\n"),
        );
        let name = format!("atomics-{version}-{INSTALL_TARGET}.tar.gz");
        let base = format!("github.com/xmagit/ATOMICS/releases/download/v{version}");
        self.feed(&format!("{base}/{name}"), archive);
        self.feed(&format!("{base}/{name}.sha256"), sha256_line(archive, &name));
        self.feed(&format!("{base}/{name}.sig"), self.key.sign(archive));
    }

    fn env<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        let path = std::env::join_paths([self.path("stubs"), self.path("tools")]).unwrap();
        command
            .env_clear()
            .env("HOME", self.home())
            .env("PATH", path)
            .env("SHELL", self.path("stubs/shell"))
            .env("FEED", self.path("feed"))
            .env("XDG_RUNTIME_DIR", self.path("runtime"))
            .current_dir(self.home())
            .stdin(Stdio::null())
    }

    fn install_script(&self) -> Output {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/install.sh");
        test_support::output(self.env(Command::new("sh").arg(script)))
    }

    fn installed(&self) -> PathBuf {
        self.home().join(".local/bin/atomics")
    }

    fn atomics(&self, args: &[&str]) -> Output {
        test_support::output(self.env(Command::new(self.installed()).args(args)))
    }

    /// What the person has, as it is now. The keychain's call log is the fixture's, and no item.
    fn what_they_have(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(path: &Path, into: &mut BTreeMap<PathBuf, Vec<u8>>) {
            if path.is_dir() {
                for entry in std::fs::read_dir(path).unwrap() {
                    walk(&entry.unwrap().path(), into);
                }
            } else if path.is_file() {
                into.insert(path.to_path_buf(), std::fs::read(path).unwrap());
            }
        }
        let mut files = BTreeMap::new();
        let state = self.home().join(".local/state/atomics");
        walk(&self.home().join(".config/atomics"), &mut files);
        walk(&state.join("plans"), &mut files);
        walk(&state.join("sessions.json"), &mut files);
        walk(&state.join("devices.json"), &mut files);
        for entry in std::fs::read_dir(self.path("stubs")).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if name.starts_with("keychain") && name != "keychain-argv" {
                walk(&path, &mut files);
            }
        }
        files
    }

    /// A host from the installed program, until it answers.
    fn host(&self) -> TestChild {
        let mut host = TestChild::spawn(self.env(Command::new(self.installed()).arg("host")), CHILD_TIMEOUT).unwrap();
        let deadline = Instant::now() + scaled_timeout(Duration::from_secs(30));
        loop {
            if let Some(status) = host.child.try_wait().unwrap() {
                let output = host.wait_output().unwrap();
                panic!("the host exited with {status}: {}", String::from_utf8_lossy(&output.stderr));
            }
            if self.atomics(&["ctl", r#"{"cmd":"list"}"#]).status.success() {
                return host;
            }
            assert!(Instant::now() < deadline, "the host never answered");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn stop(&self, mut host: TestChild) {
        // SAFETY: signalling the host this test started.
        assert_eq!(unsafe { libc::kill(host.child.id() as i32, libc::SIGTERM) }, 0);
        let output = host.wait_output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    fn ctl(&self, request: serde_json::Value) -> serde_json::Value {
        let output = self.atomics(&["ctl", &request.to_string()]);
        assert!(output.status.success(), "{request}: {output:?}");
        let answer: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(answer["ok"], true, "{request}: {answer}");
        answer["result"].clone()
    }
}

/// A server on this machine for the update itself, which the real program reads over HTTP. It is bound before the release
/// is described, since the release's answer names its address.
struct ReleaseServer {
    base: String,
    listener: Option<TcpListener>,
}

impl ReleaseServer {
    fn bind() -> ReleaseServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        ReleaseServer { base: format!("http://{}", listener.local_addr().unwrap()), listener: Some(listener) }
    }

    fn serve(&mut self, routes: BTreeMap<String, Vec<u8>>) {
        let listener = self.listener.take().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(&stream);
                let mut request = String::new();
                if reader.read_line(&mut request).is_err() {
                    continue;
                }
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|read| read > 2) {
                    line.clear();
                }
                let path = request.split_whitespace().nth(1).unwrap_or_default();
                let body = routes.get(path);
                let status = if body.is_some() { "200 OK" } else { "404 Not Found" };
                let body = body.map_or(&[][..], Vec::as_slice);
                let mut writer = &stream;
                let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = writer.write_all(head.as_bytes());
                let _ = writer.write_all(body);
            }
        });
    }
}

/// The release a server offers: `archive` as this platform's build of `version`, signed with `key`.
fn release_routes(
    base_placeholder: &str,
    version: &str,
    archive: &[u8],
    key: &ThrowawayKey,
) -> BTreeMap<String, Vec<u8>> {
    let name = format!("atomics-{version}-{TARGET}.tar.gz");
    let asset = |suffix: &str| serde_json::json!({"name": format!("{name}{suffix}"), "browser_download_url": format!("{base_placeholder}/{name}{suffix}")});
    let latest = serde_json::json!({
        "tag_name": format!("v{version}"),
        "assets": [asset(""), asset(".sha256"), asset(".sig")],
    });
    BTreeMap::from([
        ("/latest".to_string(), latest.to_string().into_bytes()),
        (format!("/{name}"), archive.to_vec()),
        (format!("/{name}.sha256"), sha256_line(archive, &name)),
        (format!("/{name}.sig"), key.sign(archive)),
    ])
}

/// Asserts each file the same as before, naming the first that is not.
fn assert_same(what: &str, before: &BTreeMap<PathBuf, Vec<u8>>, after: &BTreeMap<PathBuf, Vec<u8>>) {
    let names = |files: &BTreeMap<PathBuf, Vec<u8>>| files.keys().cloned().collect::<Vec<_>>();
    assert_eq!(names(before), names(after), "{what}: the same files");
    for (path, bytes) in before {
        assert!(
            bytes == &after[path],
            "{what}: {} changed:\n{}",
            path.display(),
            String::from_utf8_lossy(&after[path])
        );
    }
}

#[test]
fn a_release_installs_updates_and_rolls_back_and_nothing_of_the_persons_changes() {
    let machine = Machine::new();

    // Version 1: the real program, as a release holds it.
    let real = machine.path("real-atomics");
    std::fs::copy(env!("CARGO_BIN_EXE_atomics"), &real).unwrap();
    let _ = test_support::output(Command::new("strip").arg(&real));
    let v1 = std::fs::read(&real).unwrap();
    machine.publish(VERSION, &archive_of(machine.dir.path(), "v1", &real));
    let installed = machine.install_script();
    assert!(installed.status.success(), "{installed:?}");
    let said = String::from_utf8_lossy(&installed.stdout);
    assert!(said.contains("setup screen"), "{said}");
    assert!(std::fs::read(machine.installed()).unwrap() == v1, "the release's program is what was installed");
    let entry = std::fs::read_to_string(machine.home().join(".local/share/applications/atomics.desktop")).unwrap();
    assert!(entry.contains(&machine.installed().display().to_string()), "{entry}");
    assert!(machine.home().join(".local/share/icons/hicolor/512x512/apps/atomics.png").is_file());
    assert!(!machine.home().join(".local/bin/atomics.previous").exists(), "nothing to go back to yet");
    assert!(!machine.home().join(".config/atomics/config.toml").exists(), "the first run is the program's");

    // What a person ends up with after using it: a config and this machine's own file, plans, sessions, paired devices
    // and keychain items, each made by the real program where it can be.
    let host = machine.host();
    let config = std::fs::read_to_string(machine.home().join(".config/atomics/config.toml")).unwrap();
    assert!(config.contains("\nmode = \"bare\"\n"), "a new install runs on this machine");
    let project = machine.home().join("code/app");
    std::fs::create_dir_all(&project).unwrap();
    let git = |args: &[&str]| {
        let output = test_support::output(Command::new("git").arg("-C").arg(&project).args(args));
        assert!(output.status.success(), "{output:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    machine
        .ctl(serde_json::json!({"cmd": "plan_set", "goal": "Parse faster", "tasks": [{"id": "t1", "title": "First"}]}));
    machine.ctl(serde_json::json!({"cmd": "start", "agent": "shell", "folder": project}));
    machine.stop(host);

    let mut credential = TestChild::spawn(
        machine.env(Command::new(machine.installed()).args(["setup", "--credential", "openai"])).stdin(Stdio::piped()),
        CHILD_TIMEOUT,
    )
    .unwrap();
    credential.child.stdin.take().unwrap().write_all(b"sk-PRIVATE-kept-through-updates\n").unwrap();
    let output = credential.wait_output().unwrap();
    assert!(output.status.success(), "{output:?}");

    let config_d = machine.home().join(".config/atomics/config.d");
    std::fs::create_dir_all(&config_d).unwrap();
    std::fs::write(config_d.join("this-machine.toml"), "# This machine's own.\n[isolation]\nmode = \"bare\"\n")
        .unwrap();
    let state = machine.home().join(".local/state/atomics");
    let key = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";
    serde_json::from_value::<atomics_core::noise::PublicKey>(serde_json::json!(key)).expect("a key the host can read");
    let devices = serde_json::json!({"v": 1, "devices": [{
        "key": key, "name": "Safari on iPhone", "access": "control", "paired_ms": 1, "seen_ms": 2,
    }]});
    std::fs::write(state.join("devices.json"), serde_json::to_vec_pretty(&devices).unwrap()).unwrap();
    // The setup screen was skipped, with two connections left for later, as its buttons leave things.
    let mut store: serde_json::Value =
        serde_json::from_slice(&std::fs::read(state.join("sessions.json")).unwrap()).unwrap();
    store.as_object_mut().unwrap().remove("setup_pending");
    store["setup_later"] = serde_json::json!(["github", "azure"]);
    std::fs::write(state.join("sessions.json"), serde_json::to_vec_pretty(&store).unwrap()).unwrap();
    // Version 1 runs once more, so that every file is as the program itself writes it.
    machine.stop(machine.host());

    let before = machine.what_they_have();
    let names: Vec<String> = before.keys().map(|path| path.display().to_string()).collect();
    for wanted in ["config.toml", "config.d/this-machine.toml", "plans/", "sessions.json", "devices.json", "keychain-"]
    {
        assert!(names.iter().any(|name| name.contains(wanted)), "{wanted} in {names:?}");
    }

    // Version 2: a program that names a newer version, and is otherwise the real one, from a local feed with the
    // throwaway key. The real `atomics update` installs it.
    let v2_program = machine.path("v2-atomics");
    let v2_text = format!(
        "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'atomics {NEW_VERSION} (v{NEW_VERSION})'; exit 0; fi\nexec '{}' \"$@\"\n",
        real.display()
    );
    program(&v2_program, &v2_text);
    let v2_archive = archive_of(machine.dir.path(), "v2", &v2_program);
    let mut server = ReleaseServer::bind();
    let routes = release_routes(&server.base, NEW_VERSION, &v2_archive, &machine.key);
    server.serve(routes);
    let trust = machine.path("trusted.pub");
    std::fs::write(&trust, machine.key.pem()).unwrap();
    let update = |args: &[&str]| {
        test_support::output(
            machine
                .env(Command::new(machine.installed()).args(args))
                .env("ATOMICS_TEST_RELEASE_FEED", format!("{}/latest", server.base))
                .env("ATOMICS_TEST_RELEASE_KEY", &trust),
        )
    };
    let output = update(&["update", "--check"]);
    assert!(output.status.success() && String::from_utf8_lossy(&output.stdout).contains(NEW_VERSION), "{output:?}");
    assert!(std::fs::read(machine.installed()).unwrap() == v1, "a check changes nothing");

    let output = update(&["update"]);
    let said = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{said}\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(said.contains(&format!("Installed {NEW_VERSION}")), "{said}");
    assert_eq!(std::fs::read_to_string(machine.installed()).unwrap(), v2_text);
    assert_same("after the update", &before, &machine.what_they_have());
    let previous = machine.home().join(".local/bin/atomics.previous");
    assert!(std::fs::read(&previous).unwrap() == v1, "the previous version is kept");

    // Version 2 runs on all of it. Starting it writes nothing of theirs, and the setup screen stays away.
    let host = machine.host();
    let plans = machine.ctl(serde_json::json!({"cmd": "plan_get"}));
    assert_eq!(plans["plans"][0]["goal"], "Parse faster", "{plans}");
    machine.stop(host);
    let after = machine.what_they_have();
    let sessions = after.iter().find(|(path, _)| path.ends_with("sessions.json")).unwrap().1;
    let store: serde_json::Value = serde_json::from_slice(sessions).unwrap();
    assert!(store.get("setup_pending").is_none(), "the setup screen does not come back: {store}");
    assert_eq!(store["setup_later"], serde_json::json!(["github", "azure"]), "Connect later is still later");
    assert_same("after the new version ran", &before, &after);

    // The one way back, from the copy that was kept, as a new version that will not start would need.
    let back = test_support::output(machine.env(Command::new(&previous).arg("rollback")));
    assert!(back.status.success(), "{back:?}");
    assert!(String::from_utf8_lossy(&back.stdout).contains(VERSION), "{back:?}");
    assert!(std::fs::read(machine.installed()).unwrap() == v1);
    assert_eq!(std::fs::read_to_string(&previous).unwrap(), v2_text, "and forward again from here");
    assert_same("after going back", &before, &machine.what_they_have());
    let forward = machine.atomics(&["rollback"]);
    assert!(forward.status.success() && String::from_utf8_lossy(&forward.stdout).contains(NEW_VERSION), "{forward:?}");
    assert_eq!(std::fs::read_to_string(machine.installed()).unwrap(), v2_text);
    assert!(std::fs::read(&previous).unwrap() == v1);
    assert_same("after going forward", &before, &machine.what_they_have());

    // An update under way holds the lock, and going back waits for no one: it says so.
    let held = std::fs::File::create(state.join("update.lock")).unwrap();
    use std::os::fd::AsRawFd;
    // SAFETY: flock on the descriptor opened above.
    assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX) }, 0);
    let refused = machine.atomics(&["rollback"]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("Another update"), "{refused:?}");
    assert_eq!(std::fs::read_to_string(machine.installed()).unwrap(), v2_text);
}

/// What `install.sh` refuses, leaving nothing installed; a fake program stands in for the release's.
mod install_script {
    use super::*;

    /// A release whose program says `says`, and records being asked to install itself.
    fn release(machine: &Machine, says: &str) -> Vec<u8> {
        let fake = machine.path("fake-atomics");
        let log = machine.path("asked");
        program(
            &fake,
            &format!(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo '{says}'; exit 0; fi\necho \"$@\" >> '{}'\nmkdir -p \"$HOME/.local/bin\"\ncp \"$0\" \"$HOME/.local/bin/atomics\"\n",
                log.display()
            ),
        );
        let archive = archive_of(machine.dir.path(), "fake", &fake);
        machine.publish(VERSION, &archive);
        archive
    }

    fn refused(machine: &Machine, why: &str) {
        let output = machine.install_script();
        let said = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(!output.status.success(), "{output:?}");
        assert!(said.contains(why), "{why} in: {said}");
        assert!(!machine.installed().exists(), "nothing is installed");
        assert!(!machine.path("asked").exists(), "it never ran the program: {said}");
    }

    fn good(machine: &Machine) -> Vec<u8> {
        release(machine, &format!("atomics {VERSION} (v{VERSION})"))
    }

    #[test]
    fn a_good_release_installs_itself_and_nothing_else_runs() {
        let machine = Machine::new();
        good(&machine);
        let output = machine.install_script();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(std::fs::read_to_string(machine.path("asked")).unwrap().trim(), "install", "and no setup");
        assert!(machine.installed().is_file());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("not checked"), "{output:?}");
    }

    #[test]
    fn a_damaged_download_is_refused_before_it_is_unpacked() {
        let machine = Machine::new();
        let archive = good(&machine);
        let name = format!("atomics-{VERSION}-{INSTALL_TARGET}.tar.gz");
        let base = format!("github.com/xmagit/ATOMICS/releases/download/v{VERSION}");
        machine.feed(&format!("{base}/{name}"), [archive.as_slice(), b"tampered"].concat());
        refused(&machine, "damaged");
        machine.feed(&format!("{base}/{name}"), &archive);
        machine.feed(&format!("{base}/{name}.sha256"), sha256_line(&archive, "another-file.tar.gz"));
        refused(&machine, "names another file");
    }

    #[test]
    fn a_signature_from_any_other_key_is_refused_whatever_the_release_says_of_itself() {
        let machine = Machine::new();
        let archive = good(&machine);
        let name = format!("atomics-{VERSION}-{INSTALL_TARGET}.tar.gz");
        let base = format!("github.com/xmagit/ATOMICS/releases/download/v{VERSION}");
        // Consistent with its own checksum: only the key on the main branch tells them apart.
        machine.feed(&format!("{base}/{name}.sig"), ThrowawayKey::new().sign(&archive));
        refused(&machine, "not signed with an ATOMICS release key");
        std::fs::remove_file(machine.path("feed").join(format!("{base}/{name}.sig").replace('/', "_"))).unwrap();
        refused(&machine, "no signature");
    }

    /// The offline key is there for a release signed by hand, after the CI key was lost: `install.sh` takes that too.
    #[test]
    fn a_signature_from_the_offline_key_is_accepted_with_or_without_the_ci_key_on_the_branch() {
        let machine = Machine::new();
        let archive = good(&machine);
        let offline = ThrowawayKey::new();
        machine.feed(
            "raw.githubusercontent.com/xmagit/ATOMICS/main/assets/release-signing-key-offline.pub",
            offline.pem(),
        );
        let name = format!("atomics-{VERSION}-{INSTALL_TARGET}.tar.gz");
        let base = format!("github.com/xmagit/ATOMICS/releases/download/v{VERSION}");
        machine.feed(&format!("{base}/{name}.sig"), offline.sign(&archive));
        let output = machine.install_script();
        assert!(output.status.success(), "{output:?}");
        assert!(machine.installed().is_file());
        std::fs::remove_file(machine.installed()).unwrap();
        std::fs::remove_file(machine.path("asked")).unwrap();

        let ci = machine
            .path("feed")
            .join("raw.githubusercontent.com/xmagit/ATOMICS/main/assets/release-signing-key.pub".replace('/', "_"));
        std::fs::remove_file(ci).unwrap();
        let output = machine.install_script();
        assert!(output.status.success(), "{output:?}");
        std::fs::remove_file(machine.installed()).unwrap();
        std::fs::remove_file(machine.path("asked")).unwrap();

        // Neither of the two is still no.
        machine.feed(&format!("{base}/{name}.sig"), ThrowawayKey::new().sign(&archive));
        refused(&machine, "not signed with an ATOMICS release key");
    }

    #[test]
    fn with_no_keys_to_read_it_refuses_and_without_openssl_3_it_says_what_it_did_not_check() {
        let machine = Machine::new();
        good(&machine);
        let keys = machine
            .path("feed")
            .join("raw.githubusercontent.com/xmagit/ATOMICS/main/assets/release-signing-key.pub".replace('/', "_"));
        let pem = std::fs::read(&keys).unwrap();
        std::fs::remove_file(&keys).unwrap();
        refused(&machine, "could not read the release keys");
        std::fs::write(&keys, pem).unwrap();

        // As on a Mac, whose own openssl is LibreSSL.
        program(&machine.path("stubs/openssl"), "#!/bin/sh\necho 'LibreSSL 3.3.6'\n");
        let output = machine.install_script();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("signature was not checked"), "{output:?}");
    }

    #[test]
    fn a_program_that_is_not_the_version_it_claims_or_does_not_run_is_refused() {
        let machine = Machine::new();
        release(&machine, "atomics 0.0.1 (old)");
        refused(&machine, "holds a program that says");

        let fake = machine.path("fake-atomics");
        program(&fake, "#!/bin/sh\necho 'version GLIBC_2.99 not found' >&2\nexit 1\n");
        let archive = archive_of(machine.dir.path(), "broken", &fake);
        machine.publish(VERSION, &archive);
        refused(&machine, "does not run on this machine");
    }

    #[test]
    fn a_machine_with_no_build_says_so_and_a_version_can_be_asked_for() {
        let machine = Machine::new();
        good(&machine);
        program(&machine.path("stubs/uname"), "#!/bin/sh\n[ \"$1\" = -s ] && echo Linux || echo riscv64\n");
        refused(&machine, "no build for Linux riscv64");
        Machine::installer_platform(machine.dir.path());

        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/install.sh");
        let output = test_support::output(machine.env(Command::new("sh").arg(&script)).env("ATOMICS_VERSION", "9.9.9"));
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("release v9.9.9 has no build"), "{output:?}");
    }
}
