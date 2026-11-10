//! Every key here is made at test time: no key, real or placeholder, is a trust anchor of any test.

use super::test_support::*;
use super::*;
use std::os::unix::fs::MetadataExt;

fn found_on(server: &Server, key: &TestKey, target: InstallTarget) -> (Feed, Found) {
    let feed = server.feed(Ok(key.raw()), target);
    let found = feed.latest("1.0.0").unwrap().expect("a newer release");
    (feed, found)
}

/// What installing fails with, as the user reads it.
fn failure(feed: &Feed, found: &Found) -> String {
    format!("{:#}", feed.install(found, &|_| {}).unwrap_err())
}

#[test]
fn versions_compare_as_numbers_and_a_pre_release_is_never_offered() {
    assert!(is_newer("0.1.6", "v0.1.7"));
    assert!(is_newer("0.1.9", "0.1.10"));
    assert!(is_newer("0.9.9", "v1.0.0"));
    assert!(!is_newer("0.1.7", "v0.1.7"));
    assert!(!is_newer("0.1.7", "v0.1.6"));
    assert!(!is_newer("0.1.6", "v0.1.7-rc1"));
    assert!(!is_newer("0.1.6", "v0.1"));
    assert!(!is_newer("0.1.6", "v0.1.7.1"));
    assert!(!is_newer("a build", "v0.1.7"));
}

#[test]
fn the_public_key_is_read_from_its_pem_and_anything_else_is_refused() {
    let key = TestKey::new();
    assert_eq!(parse_public_key(&key.pem()).unwrap(), key.raw());
    for bad in [
        String::new(),
        "not a key".into(),
        "-----BEGIN PUBLIC KEY-----\n!!!\n-----END PUBLIC KEY-----\n".into(),
        // An RSA-sized body, and the right prefix on a short key.
        format!("-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n", "QUJD"),
        key.pem().replace("MCowBQYDK2Vw", "MCowBQYDK2Vx"),
    ] {
        assert!(parse_public_key(&bad).unwrap_err().contains("cannot be read"), "{bad}");
    }
}

#[test]
fn the_keys_built_into_this_binary_are_ones_it_can_use_and_not_one_key_twice() {
    // Absent until the maintainer provisions them; then every test run proves the files parse.
    match built_in_keys(BUILT_IN_KEYS) {
        Ok(keys) => {
            assert!((1..=2).contains(&keys.len()));
            assert!(keys.first() != keys.get(1), "the offline key is there for when the other is lost");
        }
        Err(why) => assert_eq!(why, NO_KEY),
    }
}

#[test]
fn every_key_built_in_has_to_be_readable_or_updates_are_off() {
    let (first, second) = (TestKey::new(), TestKey::new());
    let (first_pem, second_pem) = (first.pem(), second.pem());
    assert_eq!(built_in_keys(&[]).unwrap_err(), NO_KEY);
    assert_eq!(built_in_keys(&[&first_pem]).unwrap(), [first.raw()]);
    assert_eq!(built_in_keys(&[&first_pem, &second_pem]).unwrap(), [first.raw(), second.raw()]);
    assert!(built_in_keys(&[&first_pem, "not a key"]).unwrap_err().contains("cannot be read"));
    assert!(built_in_keys(&["not a key", &second_pem]).unwrap_err().contains("cannot be read"));
}

#[test]
fn a_signature_from_either_key_is_accepted_and_from_any_other_is_not() {
    let (ci, offline, other) = (TestKey::new(), TestKey::new(), TestKey::new());
    let keys = [ci.raw(), offline.raw()];
    let archive = b"archive bytes".as_slice();
    let checksum = sha256_line(archive, ARCHIVE_NAME);
    verify(archive, ARCHIVE_NAME, &checksum, &ci.sign(archive), &keys).unwrap();
    verify(archive, ARCHIVE_NAME, &checksum, &offline.sign(archive), &keys).unwrap();
    let error = verify(archive, ARCHIVE_NAME, &checksum, &other.sign(archive), &keys).unwrap_err();
    assert!(error.to_string().contains("not signed"), "{error}");
    // No keys trusts nothing.
    assert!(verify(archive, ARCHIVE_NAME, &checksum, &ci.sign(archive), &[]).is_err());

    // A release signed offline installs on a build that holds both keys, all the way through.
    let install = Install::new();
    let server = Server::start();
    let release = install.archive(&[("atomics", NEW)]);
    server.release(&release, &sha256_line(&release, ARCHIVE_NAME), &offline.sign(&release));
    let target = InstallTarget::Binary(install.target.clone());
    let feed = Feed { keys: Ok(keys.to_vec()), ..server.feed(Ok(ci.raw()), target.clone()) };
    let found = feed.latest("1.0.0").unwrap().unwrap();
    feed.install(&found, &|_| {}).unwrap();
    assert_eq!(std::fs::read_to_string(&install.target).unwrap(), NEW);

    // And not on a build that trusts only the CI key.
    let again = Install::new();
    let only_ci = server.feed(Ok(ci.raw()), InstallTarget::Binary(again.target.clone()));
    assert!(failure(&only_ci, &only_ci.latest("1.0.0").unwrap().unwrap()).contains("not signed"));
    again.untouched();
}

#[test]
fn verification_takes_the_checksum_and_the_signature_over_the_exact_bytes() {
    let key = TestKey::new();
    let archive = b"archive bytes".as_slice();
    let checksum = sha256_line(archive, ARCHIVE_NAME);
    let signature = key.sign(archive);
    verify(archive, ARCHIVE_NAME, &checksum, &signature, &[key.raw()]).unwrap();

    type Case<'a> = (&'a [u8], &'a [u8], &'a [u8], &'a str);
    let cases: [Case; 6] = [
        (b"archive bytes!", &checksum, &signature, "damaged"),
        (archive, &sha256_line(b"other", ARCHIVE_NAME), &signature, "damaged"),
        (archive, &sha256_line(archive, "another-file.tar.gz"), &signature, "names another file"),
        (archive, b"\xff\xfe", &signature, "cannot be read"),
        (archive, &checksum, &signature[..63], "not signed"),
        (archive, &checksum, &TestKey::new().sign(archive), "not signed"),
    ];
    for (bytes, sums, signed, expected) in cases {
        let error = verify(bytes, ARCHIVE_NAME, sums, signed, &[key.raw()]).unwrap_err().to_string();
        assert!(error.contains(expected), "{expected}: {error}");
    }
    // The signature of a tampered archive whose checksum was fixed up, with the key this binary trusts.
    let tampered = b"tampered".as_slice();
    let error =
        verify(tampered, ARCHIVE_NAME, &sha256_line(tampered, ARCHIVE_NAME), &signature, &[key.raw()]).unwrap_err();
    assert!(error.to_string().contains("not signed"), "{error}");
}

#[test]
fn a_check_finds_a_newer_release_says_nothing_otherwise_and_fails_in_plain_words() {
    let install = Install::new();
    let server = Server::start();
    let feed = server.feed(Ok(TestKey::new().raw()), InstallTarget::Binary(install.target.clone()));

    assert_eq!(feed.latest("1.0.0").unwrap(), None, "no release yet: a 404 is no failure");
    server.release(b"archive", b"sum", b"sig");
    let found = feed.latest("1.0.0").unwrap().expect("9.0.0 is newer");
    assert_eq!(found.version, "9.0.0");
    assert_eq!(found.notes.as_deref(), Some("https://example.com/notes/9.0.0"));
    assert!(found.assets.is_some());
    assert_eq!(Mode::Releases(feed.clone()).unavailable(&found), None);
    assert_eq!(feed.latest("9.0.0").unwrap(), None);
    assert_eq!(feed.latest("10.0.0").unwrap(), None);

    server.put("/latest", 403, "");
    assert!(format!("{:#}", feed.latest("1.0.0").unwrap_err()).contains("limiting requests"));
    server.put("/latest", 500, "");
    assert!(format!("{:#}", feed.latest("1.0.0").unwrap_err()).contains("answered 500"));
    server.put("/latest", 200, "{not json");
    assert!(format!("{:#}", feed.latest("1.0.0").unwrap_err()).contains("could not be read"));
}

#[test]
fn a_release_without_this_platform_is_found_and_cannot_be_installed() {
    let install = Install::new();
    let server = Server::start();
    let feed = server.feed(Ok(TestKey::new().raw()), InstallTarget::Binary(install.target.clone()));
    server.put("/latest", 200, serde_json::json!({"tag_name":"v9.0.0","assets":[]}).to_string());
    let found = feed.latest("1.0.0").unwrap().unwrap();
    let why = Mode::Releases(feed.clone()).unavailable(&found).unwrap();
    assert!(why.contains("no build for test-platform"), "{why}");
    assert!(failure(&feed, &found).contains("no build for test-platform"));
    install.untouched();
}

#[test]
fn with_no_key_built_in_it_finds_a_release_says_so_and_downloads_nothing() {
    let install = Install::new();
    let server = Server::start();
    let key = TestKey::new();
    let archive = install.archive(&[("atomics", NEW)]);
    server.release(&archive, &sha256_line(&archive, ARCHIVE_NAME), &key.sign(&archive));
    let feed = server.feed(Err(NO_KEY.into()), InstallTarget::Binary(install.target.clone()));
    let found = feed.latest("1.0.0").unwrap().expect("the check still works");
    assert_eq!(Mode::Releases(feed.clone()).unavailable(&found).as_deref(), Some(NO_KEY));
    // A correctly signed archive changes nothing: there is no key to check it with.
    server.put(&format!("/{ARCHIVE_NAME}"), 500, "");
    assert_eq!(failure(&feed, &found), NO_KEY);
    install.untouched();
}

#[test]
fn a_verified_release_replaces_the_program_with_a_new_file() {
    let install = Install::new();
    let server = Server::start();
    let key = TestKey::new();
    let archive = install.archive(&[("atomics", NEW)]);
    server.release(&archive, &sha256_line(&archive, ARCHIVE_NAME), &key.sign(&archive));
    let (feed, found) = found_on(&server, &key, InstallTarget::Binary(install.target.clone()));

    let inode = std::fs::metadata(&install.target).unwrap().ino();
    // A process that runs the old file holds it open: a rename replaces it where a write would fail with ETXTBSY.
    let mut running = Command::new(&install.target).stdout(Stdio::null()).spawn().unwrap();
    let steps = std::cell::RefCell::new(Vec::new());
    feed.install(&found, &|step| steps.borrow_mut().push(step)).unwrap();
    let _ = running.wait();
    assert_eq!(std::fs::read_to_string(&install.target).unwrap(), NEW);
    assert_ne!(std::fs::metadata(&install.target).unwrap().ino(), inode, "a new file, not the running one rewritten");
    assert_eq!(*steps.borrow(), ["Downloading 9.0.0", "Verifying 9.0.0", "Installing 9.0.0"]);
    install.no_leftovers();
    // What it replaced is kept, the very file a running program holds.
    let previous = install.target.with_file_name("atomics.previous");
    assert_eq!(std::fs::read_to_string(&previous).unwrap(), OLD);
    assert_eq!(std::fs::metadata(&previous).unwrap().ino(), inode);
    use std::os::unix::fs::PermissionsExt;
    assert!(std::fs::metadata(&previous).unwrap().permissions().mode() & 0o111 != 0, "and it still runs");
}

#[test]
fn only_a_debug_build_can_name_another_feed_and_the_release_profile_is_not_one() {
    let manifest = include_str!("../../../../Cargo.toml");
    let release = manifest.split("[profile.release]").nth(1).unwrap();
    let release = release.split("\n[").next().unwrap();
    assert!(!release.contains("debug-assertions"), "the test feed would reach a release: {release}");
}

#[test]
fn going_back_trades_places_from_either_copy_and_refuses_what_cannot_run() {
    let dir = crate::test_support::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let (target, previous) = (bin.join("atomics"), bin.join("atomics.previous"));
    let (old, new) = ("#!/bin/sh\necho 'atomics 1.0.0 (v1)'\n", "#!/bin/sh\necho 'atomics 2.0.0 (v2)'\n");
    crate::test_support::write_program(&target, new);

    let error = rollback(&target, false).unwrap_err();
    assert!(format!("{error:#}").contains("No previous version"), "{error:#}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), new);

    crate::test_support::write_program(&previous, old);
    assert_eq!(rollback(&target, false).unwrap(), "1.0.0");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), old);
    assert_eq!(std::fs::read_to_string(&previous).unwrap(), new);
    // Forward again, and from the kept copy, as a new version that will not start would need.
    assert_eq!(rollback(&previous, false).unwrap(), "2.0.0");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), new);
    assert_eq!(std::fs::read_to_string(&previous).unwrap(), old);

    // A kept copy that does not run here stays out.
    crate::test_support::write_program(&previous, "#!/bin/sh\nexit 1\n");
    assert!(rollback(&target, false).is_err());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), new);

    // With the new version gone altogether, the kept copy still puts itself back.
    crate::test_support::write_program(&previous, old);
    std::fs::remove_file(&target).unwrap();
    assert_eq!(rollback(&previous, false).unwrap(), "1.0.0");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), old);
    assert_eq!(std::fs::read_dir(&bin).unwrap().count(), 1, "nothing else is left");
}

#[test]
fn trading_places_without_the_systems_help_puts_things_back_if_it_fails() {
    let dir = crate::test_support::tempdir().unwrap();
    let (previous, target, aside) = (dir.path().join("previous"), dir.path().join("target"), dir.path().join("aside"));
    std::fs::write(&target, "kept").unwrap();
    assert!(trade_places(&previous, &target, &aside).is_err(), "there is no previous to trade with");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "kept");
    assert!(!aside.exists());
}

#[test]
fn a_kept_bundle_goes_back_whole_under_a_name_macos_does_not_offer() {
    let dir = crate::test_support::tempdir().unwrap();
    let folder = dir.path().join("Applications");
    let bundle = |name: &str, text: &str| {
        let program = folder.join(name).join("Contents/MacOS/atomics");
        std::fs::create_dir_all(program.parent().unwrap()).unwrap();
        crate::test_support::write_program(&program, text);
        program
    };
    let program = bundle("ATOMICS.app", "#!/bin/sh\necho 'atomics 2.0.0 (v2)'\n");
    let kept = bundle(".atomics-previous", "#!/bin/sh\necho 'atomics 1.0.0 (v1)'\n");
    assert_eq!(rollback(&program, true).unwrap(), "1.0.0");
    assert!(std::fs::read_to_string(&program).unwrap().contains("1.0.0"));
    assert!(std::fs::read_to_string(&kept).unwrap().contains("2.0.0"));
    assert_eq!(rollback(&kept, true).unwrap(), "2.0.0", "and from the kept copy");
    assert!(std::fs::read_to_string(&program).unwrap().contains("2.0.0"));
}

#[test]
fn any_verification_failure_leaves_the_old_install_and_says_why() {
    let install = Install::new();
    let server = Server::start();
    let key = TestKey::new();
    let archive = install.archive(&[("atomics", NEW)]);
    let (checksum, signature) = (sha256_line(&archive, ARCHIVE_NAME), key.sign(&archive));
    server.release(&archive, &checksum, &signature);
    let (feed, found) = found_on(&server, &key, InstallTarget::Binary(install.target.clone()));
    let path = |suffix: &str| format!("/{ARCHIVE_NAME}{suffix}");

    let mut flipped = archive.clone();
    *flipped.last_mut().unwrap() ^= 1;
    server.put(&path(""), 200, flipped.clone());
    assert!(failure(&feed, &found).contains("damaged"));
    install.untouched();
    server.put(&path(".sha256"), 200, sha256_line(&flipped, ARCHIVE_NAME));
    assert!(failure(&feed, &found).contains("not signed"), "a checksum alone proves nothing");
    install.untouched();
    server.put(&path(""), 200, archive.clone());
    server.put(&path(".sha256"), 200, checksum.clone());
    server.put(&path(".sig"), 200, TestKey::new().sign(&archive));
    assert!(failure(&feed, &found).contains("not signed"), "another key");
    install.untouched();
    server.put(&path(".sig"), 200, signature.clone());

    // Each says what was kept.
    server.put(&path(""), 404, "");
    let error = failure(&feed, &found);
    assert!(error.contains("Could not download the release"), "{error}");
    assert!(error.contains("The installed version was not changed."), "{error}");
    install.untouched();
}

#[test]
fn a_signed_archive_that_cannot_be_installed_is_refused_before_anything_is_replaced() {
    let install = Install::new();
    let server = Server::start();
    let key = TestKey::new();
    let sign = |server: &Server, archive: &[u8]| {
        server.release(archive, &sha256_line(archive, ARCHIVE_NAME), &key.sign(archive));
    };
    sign(&server, &install.archive(&[("atomics", NEW)]));
    let (feed, found) = found_on(&server, &key, InstallTarget::Binary(install.target.clone()));

    sign(&server, b"not a gzip");
    assert!(failure(&feed, &found).contains("could not be unpacked"));
    install.untouched();
    sign(&server, &install.archive(&[("something-else", NEW)]));
    assert!(failure(&feed, &found).contains("does not hold atomics"));
    install.untouched();
    sign(&server, &install.archive(&[("atomics", "#!/bin/sh\nexit 1\n")]));
    assert!(failure(&feed, &found).contains("does not run on this machine"));
    install.untouched();
}

#[test]
fn the_version_is_the_first_word_after_the_name() {
    assert_eq!(reported_version("atomics 0.1.7 (v0.1.7)\n"), Some("0.1.7"));
    assert_eq!(reported_version("atomics 0.1.7 (v0.1.7-3-gabc1234-dirty)\n"), Some("0.1.7"));
    assert_eq!(reported_version("atomics 0.1.7"), Some("0.1.7"));
    assert_eq!(reported_version("something 0.1.7 (v0.1.7)\n"), None);
    assert_eq!(reported_version("0.1.7\n"), None);
    assert_eq!(reported_version("\n"), None);
    assert_eq!(reported_version(""), None);
}

#[test]
fn an_older_signed_build_offered_under_a_newer_tag_is_refused() {
    let install = Install::new();
    let bundle = install.dir.path().join("Applications/ATOMICS.app");
    let in_bundle = bundle.join("Contents/MacOS/atomics");
    std::fs::create_dir_all(in_bundle.parent().unwrap()).unwrap();
    crate::test_support::write_program(&in_bundle, OLD);
    let server = Server::start();
    let key = TestKey::new();
    let inside = "ATOMICS.app/Contents/MacOS/atomics";

    // Each is genuinely signed with the key this binary trusts, and listed as 9.0.0.
    for said in [
        "echo 'atomics 1.0.0 (v1.0.0)'",
        "echo 'atomics 9.0.01 (v9.0.01)'",
        "echo 'atomics 9.0.0-rc1 (v9.0.0-rc1)'",
        // As builds before this check print it.
        "echo 'atomics v1.0.0'",
        "echo 'atomics 61ffd46'",
        "echo 'something else'",
        "true",
    ] {
        let program = format!("#!/bin/sh\n{said}\n");
        for (target, entry) in [
            (InstallTarget::Binary(install.target.clone()), "atomics"),
            (InstallTarget::Bundle(bundle.clone()), inside),
        ] {
            let archive = install.archive(&[(entry, &program)]);
            server.release(&archive, &sha256_line(&archive, ARCHIVE_NAME), &key.sign(&archive));
            let (feed, found) = found_on(&server, &key, target);
            let error = failure(&feed, &found);
            assert!(error.contains("not the version it claims"), "{said}: {error}");
            assert!(error.contains("Release 9.0.0"), "{said}: {error}");
            assert!(error.contains("The installed version was not changed."), "{said}: {error}");
        }
        install.untouched();
        assert_eq!(std::fs::read_to_string(&in_bundle).unwrap(), OLD, "{said}");
    }
    assert_eq!(std::fs::read_dir(bundle.parent().unwrap()).unwrap().count(), 1, "nothing is left beside the bundle");

    // The same keys and archive shape, with the version the release names, goes in.
    let archive = install.archive(&[("atomics", NEW)]);
    server.release(&archive, &sha256_line(&archive, ARCHIVE_NAME), &key.sign(&archive));
    let (feed, found) = found_on(&server, &key, InstallTarget::Binary(install.target.clone()));
    feed.install(&found, &|_| {}).unwrap();
    assert_eq!(std::fs::read_to_string(&install.target).unwrap(), NEW);
}

#[test]
fn a_swap_that_fails_keeps_what_was_there_and_says_why() {
    let install = Install::new();
    let server = Server::start();
    let key = TestKey::new();
    let archive = install.archive(&[("atomics", NEW)]);
    server.release(&archive, &sha256_line(&archive, ARCHIVE_NAME), &key.sign(&archive));

    // A rename cannot put a file over a folder that has something in it.
    let occupied = install.dir.path().join("occupied");
    std::fs::create_dir_all(occupied.join("atomics/kept")).unwrap();
    let (feed, found) = found_on(&server, &key, InstallTarget::Binary(occupied.join("atomics")));
    let error = failure(&feed, &found);
    assert!(error.contains("Could not replace"), "{error}");
    assert!(occupied.join("atomics/kept").is_dir());
    assert_eq!(std::fs::read_dir(&occupied).unwrap().count(), 1, "nothing is left beside it");

    // A folder this user cannot write to fails before the download is unpacked.
    let locked = install.dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    crate::test_support::write_program(locked.join("atomics"), OLD);
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        let (feed, found) = found_on(&server, &key, InstallTarget::Binary(locked.join("atomics")));
        let error = failure(&feed, &found);
        assert!(error.contains("Could not write to"), "{error}");
        assert!(error.contains("not changed"), "{error}");
        assert_eq!(std::fs::read_to_string(locked.join("atomics")).unwrap(), OLD);
    }
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn a_bundle_is_swapped_whole_and_kept_when_the_new_one_is_broken() {
    let install = Install::new();
    let bundle = install.dir.path().join("Applications/ATOMICS.app");
    let program = bundle.join("Contents/MacOS/atomics");
    std::fs::create_dir_all(program.parent().unwrap()).unwrap();
    crate::test_support::write_program(&program, OLD);
    std::fs::write(bundle.join("Contents/old-only"), "old").unwrap();

    let server = Server::start();
    let key = TestKey::new();
    let inside = "ATOMICS.app/Contents/MacOS/atomics";
    let broken = install.archive(&[(inside, "#!/bin/sh\nexit 1\n")]);
    server.release(&broken, &sha256_line(&broken, ARCHIVE_NAME), &key.sign(&broken));
    let (feed, found) = found_on(&server, &key, InstallTarget::Bundle(bundle.clone()));
    assert!(failure(&feed, &found).contains("does not run"));
    assert_eq!(std::fs::read_to_string(&program).unwrap(), OLD);

    let archive = install.archive(&[(inside, NEW)]);
    server.release(&archive, &sha256_line(&archive, ARCHIVE_NAME), &key.sign(&archive));
    feed.install(&found, &|_| {}).unwrap();
    assert_eq!(std::fs::read_to_string(&program).unwrap(), NEW);
    assert!(!bundle.join("Contents/old-only").exists(), "the bundle is replaced whole");
    let names: Vec<_> =
        std::fs::read_dir(bundle.parent().unwrap()).unwrap().map(|entry| entry.unwrap().file_name()).collect();
    assert_eq!(names.len(), 2, "nothing is left beside it but what it keeps: {names:?}");
    let kept = bundle.with_file_name(".atomics-previous");
    assert_eq!(std::fs::read_to_string(kept.join("Contents/MacOS/atomics")).unwrap(), OLD);
    assert!(kept.join("Contents/old-only").is_file(), "whole");
}

#[test]
fn a_bundle_swap_that_fails_at_the_second_rename_puts_the_old_one_back() {
    let dir = crate::test_support::tempdir().unwrap();
    let (target, aside) = (dir.path().join("ATOMICS.app"), dir.path().join("previous.app"));
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("kept"), "old").unwrap();
    assert!(swap_bundle(&dir.path().join("missing.app"), &target, &aside).is_err());
    assert!(target.join("kept").is_file());
    assert!(!aside.exists());
}

#[test]
fn what_gets_replaced_is_the_installed_file_and_never_a_development_build() {
    let dir = crate::test_support::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let installed = root.join("bin/atomics");
    std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
    std::fs::write(&installed, "").unwrap();
    let link = root.join("link");
    std::os::unix::fs::symlink(&installed, &link).unwrap();
    assert_eq!(InstallTarget::of(&link, false).unwrap(), InstallTarget::Binary(installed.clone()));

    std::fs::write(root.join("Cargo.toml"), "").unwrap();
    let built = root.join("target/release/atomics");
    std::fs::create_dir_all(built.parent().unwrap()).unwrap();
    std::fs::write(&built, "").unwrap();
    assert!(format!("{:#}", InstallTarget::of(&built, false).unwrap_err()).contains("development build"));

    let in_bundle = root.join("ATOMICS.app/Contents/MacOS/atomics");
    std::fs::create_dir_all(in_bundle.parent().unwrap()).unwrap();
    std::fs::write(&in_bundle, "").unwrap();
    assert_eq!(InstallTarget::of(&in_bundle, true).unwrap(), InstallTarget::Bundle(root.join("ATOMICS.app")));
    assert!(format!("{:#}", InstallTarget::of(&installed, true).unwrap_err()).contains("not running from an app"));

    // A folder that only happens to end in `.app` is not a bundle, and an update must not swap it away.
    std::fs::create_dir_all(root.join("work.app/target/debug")).unwrap();
    for program in [
        "work.app/target/debug/atomics",
        "work.app/atomics",
        "ATOMICS.app/Contents/MacOS/something-else",
        "ATOMICS.app/Contents/atomics",
    ] {
        let program = root.join(program);
        std::fs::write(&program, "").unwrap();
        let error = format!("{:#}", InstallTarget::of(&program, true).unwrap_err());
        assert!(error.contains("not running from an app"), "{}: {error}", program.display());
    }
}

#[test]
fn what_goes_under_the_real_name_is_on_the_disk_first() {
    let dir = crate::test_support::tempdir().unwrap();
    let bundle = dir.path().join("ATOMICS.app");
    std::fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
    std::fs::write(bundle.join("Contents/MacOS/atomics"), "program").unwrap();
    std::os::unix::fs::symlink("nowhere", bundle.join("Contents/link")).unwrap();
    on_disk(&bundle).unwrap();
    on_disk(&bundle.join("Contents/MacOS/atomics")).unwrap();
    assert!(on_disk(&dir.path().join("missing")).is_err(), "what is not there is not on the disk");
}

#[test]
fn unpacking_gives_the_archives_owner_no_say() {
    let tar = unpack_command(Path::new("archive"), Path::new("folder"));
    assert!(tar.get_args().any(|arg| arg == "--no-same-owner"), "{tar:?}");
}

#[test]
fn audit_p7_staging_cleanup_preserves_current_and_legacy_rollback_asides() {
    let dir = crate::test_support::tempdir().unwrap();
    let stale = dir.path().join(format!("{STAGING}download"));
    std::fs::create_dir(&stale).unwrap();
    for prefix in [ROLLBACK_ASIDE.to_string(), format!("{STAGING}rollback-")] {
        let aside = dir.path().join(format!("{prefix}crashed"));
        std::fs::create_dir(&aside).unwrap();
        std::fs::write(aside.join("program"), b"recoverable install").unwrap();
    }
    remove_stale_staging(dir.path());
    assert!(!stale.exists());
    for prefix in [ROLLBACK_ASIDE.to_string(), format!("{STAGING}rollback-")] {
        assert_eq!(std::fs::read(dir.path().join(format!("{prefix}crashed/program"))).unwrap(), b"recoverable install");
    }
}

#[test]
fn a_crash_leaves_nothing_the_next_update_keeps_and_only_one_update_runs_at_a_time() {
    const LOCK_HOME: &str = "ATOMICS_UPDATE_LOCK_TEST_HOME";
    const PROBE: &str = "ATOMICS_UPDATE_LOCK_TEST_PROBE";
    const TEST: &str = "update::tests::a_crash_leaves_nothing_the_next_update_keeps_and_only_one_update_runs_at_a_time";
    if let Some(home) = std::env::var_os(LOCK_HOME) {
        let home = Path::new(&home);
        if std::env::var_os(PROBE).is_some() {
            assert_eq!(lock(home).err().unwrap().to_string(), "Another update is under way.");
            return;
        }
        let held = lock(home).unwrap();
        assert_eq!(lock(home).err().unwrap().to_string(), "Another update is under way.");
        let contender = crate::test_support::output(
            Command::new(std::env::current_exe().unwrap()).args(["--exact", TEST, "--nocapture"]).env(PROBE, "1"),
        );
        assert!(contender.status.success(), "{contender:?}");
        drop(held);
        lock(home).expect("the next update acquires the released lock without retrying");
        return;
    }
    let install = Install::new();
    let stale = install.target.parent().unwrap().join(format!("{STAGING}crashed"));
    std::fs::create_dir(&stale).unwrap();
    let server = Server::start();
    let key = TestKey::new();
    let archive = install.archive(&[("atomics", NEW)]);
    server.release(&archive, &sha256_line(&archive, ARCHIVE_NAME), &key.sign(&archive));
    let (feed, found) = found_on(&server, &key, InstallTarget::Binary(install.target.clone()));
    feed.install(&found, &|_| {}).unwrap();
    install.no_leftovers();

    let home = install.dir.path().join("state");
    // A parallel test's fork can inherit a held flock until exec, keeping it locked
    // after this test drops it. Isolate the lock lifetime from those unrelated forks.
    // The child still checks exclusion in this process and in a second process.
    let owner = crate::test_support::output(
        Command::new(std::env::current_exe().unwrap()).args(["--exact", TEST, "--nocapture"]).env(LOCK_HOME, &home),
    );
    assert!(owner.status.success(), "{owner:?}");
}
