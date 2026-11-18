use std::cell::Cell;

use super::*;

fn bundle(admin: bool) -> BundleInfo {
    let mut info = serde_json::json!({
        "version": 1, "instance": "vm-test", "user": "atomics", "home": "/home/atomics",
        "macUser": "alice", "installDir": "/bundle", "host": {"port": 47800}
    });
    if admin {
        info["admin"] = "admin".into();
    }
    serde_json::from_value(info).unwrap()
}

fn record(hardened: bool, modules: &[(&str, &str)]) -> Record {
    Record {
        version: 1,
        hardened,
        modules: modules
            .iter()
            .map(|(name, sha256)| Digest { name: (*name).into(), sha256: (*sha256).into() })
            .collect(),
    }
}

fn guest(running: Option<Record>, wheel: bool) -> Guest {
    Guest { next_boot: running.clone(), running, wheel, admin: true }
}

fn module(name: &str, sha256: &str) -> Configured {
    Configured { name: name.into(), sha256: Ok(sha256.into()) }
}

#[test]
fn configured_modules_are_named_by_their_real_path_and_hashed_as_nix_hashes_them() {
    let dir = crate::test_support::tempdir().unwrap();
    let file = dir.path().join("team.nix");
    std::fs::write(&file, "{ }\n").unwrap();
    let modules = configured(&[file.display().to_string(), dir.path().join("missing.nix").display().to_string()]);
    assert_eq!(modules[0].name, file.canonicalize().unwrap().display().to_string());
    // `builtins.hashFile "sha256"` prints lowercase hex, as `printf '{ }\n' | sha256sum` does.
    assert_eq!(modules[0].sha256.as_deref(), Ok("1d6faa9e1a76d13f3ab8558a3640158b1f0a54f624a4e37ddc3ef41ed4191058"));
    assert!(modules[1].sha256.is_err(), "a module that is not there cannot be read");
}

/// The four cases of the comparison: no record, the same modules, changed modules, and hardened mode configured but
/// not active.
#[test]
fn no_record_says_so_only_when_modules_are_configured() {
    let info = bundle(false);
    let none = Report::of(&[], &info, &guest(None, true));
    assert_eq!(none, Report::Matches { modules: 0, hardened: false });
    let drift = Report::of(&[module("/repos/team/team.nix", "aa")], &info, &guest(None, true));
    assert!(drift.is_drift());
    assert_eq!(drift.fix(), Some("atomics vm update"));
    let details = drift.details();
    assert_eq!(details.len(), 1, "{details:?}");
    assert!(details[0].contains("does not record its [vm] modules"), "{details:?}");
}

#[test]
fn the_same_modules_match_whatever_their_order() {
    let built = record(false, &[("/repos/b.nix", "bb"), ("/repos/a.nix", "aa")]);
    let report = Report::of(
        &[module("/repos/a.nix", "aa"), module("/repos/b.nix", "bb")],
        &bundle(false),
        &guest(Some(built), true),
    );
    assert_eq!(report, Report::Matches { modules: 2, hardened: false });
    assert_eq!(report.headline(), "The VM runs the system this Mac configures: 2 [vm] modules, hardened mode off");
    assert_eq!(report.fix(), None);
}

#[test]
fn changed_added_removed_and_unreadable_modules_each_have_a_line() {
    let built = record(false, &[("/repos/kept.nix", "11"), ("/repos/edited.nix", "22"), ("/repos/dropped.nix", "33")]);
    let configured = [
        module("/repos/kept.nix", "11"),
        module("/repos/edited.nix", "2f"),
        module("/repos/new.nix", "44"),
        Configured { name: "/repos/gone.nix".into(), sha256: Err("No such file or directory".into()) },
    ];
    let report = Report::of(&configured, &bundle(false), &guest(Some(built), true));
    assert_eq!(
        report.details(),
        [
            "/repos/edited.nix: changed since the running system was built",
            "/repos/new.nix: in [vm] modules, not in the running system",
            "/repos/gone.nix: in [vm] modules, but this Mac cannot read it (No such file or directory)",
            "/repos/dropped.nix: in the running system, no longer in [vm] modules",
        ]
    );
    assert_eq!(report.headline(), "The VM does not run the system this Mac configures:");
}

#[test]
fn hardened_mode_configured_but_not_active_says_why() {
    let modules = [module("/repos/team.nix", "aa")];
    let hardened = record(true, &[("/repos/team.nix", "aa")]);
    let generic = record(false, &[("/repos/team.nix", "aa")]);

    // The bundle has an admin user, but the VM's user can still use sudo.
    let in_wheel = Report::of(&modules, &bundle(true), &guest(Some(generic.clone()), true));
    assert_eq!(
        in_wheel.details(),
        ["hardened mode: configured, but not active: its user atomics is in wheel and may use sudo without a password"]
    );
    assert_eq!(in_wheel.fix(), Some(FIX));

    let built_without = Report::of(&modules, &bundle(true), &guest(Some(generic), false));
    assert_eq!(
        built_without.details(),
        ["hardened mode: configured, but not active: the running system was built without it"]
    );

    let silent_admin = Guest { admin: false, ..guest(Some(hardened.clone()), false) };
    assert_eq!(
        Report::of(&modules, &bundle(true), &silent_admin).details(),
        ["hardened mode: configured, but not active: its admin user admin did not answer over Lima's ssh"]
    );

    // The next boot records it although the installed bundle predates it.
    let staged =
        Guest { next_boot: Some(hardened.clone()), ..guest(Some(record(false, &[("/repos/team.nix", "aa")])), true) };
    assert!(Report::of(&modules, &bundle(false), &staged).is_drift());

    let active = Report::of(&modules, &bundle(true), &guest(Some(hardened), false));
    assert_eq!(active, Report::Matches { modules: 1, hardened: true });
    assert_eq!(active.headline(), "The VM runs the system this Mac configures: 1 [vm] module, hardened mode active");

    // A hardened system built before records: the VM's user is out of wheel and the admin answers.
    assert_eq!(Report::of(&[], &bundle(true), &guest(None, false)), Report::Matches { modules: 0, hardened: true });
}

#[test]
fn an_update_without_a_restart_says_the_next_boot_already_has_it() {
    let modules = [module("/repos/team.nix", "aa")];
    let staged =
        Guest { running: None, next_boot: Some(record(true, &[("/repos/team.nix", "aa")])), wheel: true, admin: false };
    let report = Report::of(&modules, &bundle(true), &staged);
    let details = report.details();
    assert_eq!(details.len(), 3, "{details:?}");
    assert!(details[1].contains("in wheel"), "{details:?}");
    assert_eq!(details[2], "its next boot is already that system: an update installed it without restarting");
    // The next boot without the module is not "already that system".
    let other = Guest { next_boot: Some(record(true, &[])), ..staged };
    assert!(!Report::of(&modules, &bundle(true), &other).details().iter().any(|line| line.contains("next boot")));
}

#[test]
fn commands_print_the_difference_and_the_fix() {
    let report = Report::of(&[module("/repos/team.nix", "aa")], &bundle(true), &guest(Some(record(false, &[])), true));
    let mut out = Vec::new();
    say(&mut out, &report).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert_eq!(
        text,
        "The VM does not run the system this Mac configures:\n\
         \x20 /repos/team.nix: in [vm] modules, not in the running system\n\
         \x20 hardened mode: configured, but not active: its user atomics is in wheel and may use sudo without a password\n\
         Fix: atomics vm update (builds the system they describe and restarts the VM into it)\n"
    );
    let mut out = Vec::new();
    say(&mut out, &Report::Unknown("the VM vm-test is stopped".into())).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "Not checked whether the VM runs the system this Mac configures: the VM vm-test is stopped\n"
    );
}

/// The guest's script, run here against a fake system: records under `running` and `next`, and an `id` that names
/// the user's groups. A login shell's greeting before the marker is ignored.
#[test]
fn the_guest_script_reads_both_records_and_the_users_groups_without_root() {
    let dir = crate::test_support::tempdir().unwrap();
    let root = dir.path();
    let running = root.join("running");
    std::fs::create_dir_all(running.join("etc/atomics")).unwrap();
    std::fs::write(
        running.join(RECORD),
        r#"{"hardened":true,"modules":[{"name":"/repos/team.nix","sha256":"aa"}],"version":1}"#,
    )
    .unwrap();
    std::fs::create_dir(root.join("bin")).unwrap();
    crate::test_support::write_program(
        root.join("bin/id"),
        "#!/bin/sh\ntest \"$1\" = -nG && test \"$2\" = \"o'brien\" || exit 1\necho users docker\n",
    );
    let script = probe_script("o'brien", running.to_str().unwrap(), root.join("next").to_str().unwrap());
    let output = Command::new("bash")
        .arg("-c")
        .arg(format!("echo 'Welcome'\n{script}"))
        .env("PATH", crate::test_support::path_with(&root.join("bin")))
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let guest = parse_guest(&String::from_utf8(output.stdout).unwrap(), true).unwrap();
    assert_eq!(guest.running, Some(record(true, &[("/repos/team.nix", "aa")])));
    assert_eq!(guest.next_boot, None, "no next boot record");
    assert!(!guest.wheel);
    assert!(parse_guest("atomics-vm-build\nnull\nnull\nusers wheel\n", false).unwrap().wheel);
    assert!(
        parse_guest("atomics-vm-build\n{\"version\":2,\"hardened\":false,\"modules\":[]}\nnull\nusers\n", false)
            .is_err()
    );
    assert!(parse_guest("null\nnull\nusers\n", false).is_err(), "no marker, no answer");
}

#[test]
fn the_admin_answers_in_hardened_mode_and_the_vms_user_only_when_it_does_not() {
    let answer = "atomics-vm-build\nnull\nnull\nusers\n";
    let asked = std::cell::RefCell::new(Vec::new());
    let run = |info: &BundleInfo| -> Result<String> {
        asked.borrow_mut().push(info.admin.clone());
        if info.admin.is_some() && asked.borrow().len() == 1 && FAIL_ADMIN.with(Cell::get) {
            bail!("Permission denied (publickey)");
        }
        Ok(answer.into())
    };
    thread_local!(static FAIL_ADMIN: Cell<bool> = const { Cell::new(false) });

    assert!(probe_with(&bundle(true), &run).unwrap().admin);
    assert_eq!(*asked.borrow(), [Some("admin".to_string())], "the VM's user is not asked when the admin answers");

    asked.borrow_mut().clear();
    FAIL_ADMIN.with(|fail| fail.set(true));
    let guest = probe_with(&bundle(true), &run).unwrap();
    assert!(!guest.admin);
    assert_eq!(*asked.borrow(), [Some("admin".to_string()), None]);

    asked.borrow_mut().clear();
    assert!(!probe_with(&bundle(false), &run).unwrap().admin);
    assert_eq!(*asked.borrow(), [None]);
}

#[test]
fn the_app_asks_the_guest_when_it_comes_up_then_once_a_minute_but_reads_the_config_every_poll() {
    let dir = crate::test_support::tempdir().unwrap();
    let file = dir.path().join("team.nix");
    std::fs::write(&file, "{ }").unwrap();
    let name = file.canonicalize().unwrap().display().to_string();
    let built = record(false, &[(&name, &sha256(b"{ }"))]);
    let asked = Cell::new(0);
    let probe = |_: &BundleInfo| -> Result<Guest> {
        asked.set(asked.get() + 1);
        Ok(guest(Some(built.clone()), true))
    };
    let modules = || Ok(vec![file.display().to_string()]);
    let mut watch = Watch::default();
    let start = Instant::now();
    let check = |watch: &mut Watch, status: Status, at: Duration| {
        watch.check_with(&status, Some(bundle(false)), start + at, &probe, &modules)
    };

    assert_eq!(check(&mut watch, Status::Stopped, Duration::ZERO), None);
    assert_eq!(asked.get(), 0, "a stopped VM is not asked");
    assert_eq!(
        check(&mut watch, Status::Running, Duration::ZERO),
        Some(Report::Matches { modules: 1, hardened: false })
    );
    std::fs::write(&file, "{ imports = [ ]; }").unwrap();
    let edited = check(&mut watch, Status::Running, Duration::from_secs(5)).unwrap();
    assert_eq!(edited.details(), [format!("{name}: changed since the running system was built")]);
    assert_eq!(asked.get(), 1, "an edit on the Mac shows without asking the guest again");
    check(&mut watch, Status::Running, Duration::from_secs(61));
    assert_eq!(asked.get(), 2, "the guest is asked again after a minute");
    check(&mut watch, Status::Stopped, Duration::from_secs(62));
    check(&mut watch, Status::Running, Duration::from_secs(63));
    assert_eq!(asked.get(), 3, "and as soon as it runs again");
    assert_eq!(watch.check_with(&Status::Running, None, start, &probe, &modules), None, "no bundle, no report");

    let failing = |_: &BundleInfo| -> Result<Guest> {
        asked.set(asked.get() + 1);
        bail!("ssh: connect to host refused")
    };
    let mut watch = Watch::default();
    let unknown = watch.check_with(&Status::Running, Some(bundle(false)), start, &failing, &modules).unwrap();
    assert_eq!(unknown, Report::Unknown("ssh: connect to host refused".into()));
    let before = asked.get();
    watch.check_with(&Status::Running, Some(bundle(false)), start + Duration::from_secs(5), &failing, &modules);
    assert_eq!(asked.get(), before);
    watch.check_with(&Status::Running, Some(bundle(false)), start + Duration::from_secs(11), &failing, &modules);
    assert_eq!(asked.get(), before + 1, "a guest that did not answer is asked again sooner");
}

#[test]
fn a_guest_that_does_not_answer_in_time_is_left() {
    let mut command = Command::new("sh");
    command.args(["-c", "exec sleep 30"]);
    let started = Instant::now();
    let error = output_within(command, Duration::from_millis(300)).unwrap_err();
    assert!(error.to_string().contains("did not answer"), "{error:#}");
    assert!(started.elapsed() < Duration::from_secs(10));
    let mut command = Command::new("sh");
    command.args(["-c", "head -c 200000 /dev/zero | tr '\\0' x"]);
    assert_eq!(output_within(command, Duration::from_secs(20)).unwrap().len(), MAX_OUTPUT as usize);
    let mut command = Command::new("sh");
    command.args(["-c", "echo no such user >&2; exit 1"]);
    assert!(output_within(command, Duration::from_secs(20)).unwrap_err().to_string().contains("no such user"));
}

#[test]
fn a_staged_user_route_needs_a_new_lima_boot_and_the_selected_generation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("atomics/accounts");
    let pid = dir.path().join("ha.pid");
    std::fs::write(&pid, "10").unwrap();
    let (admin, user) = (bundle(true), bundle(false));
    let mut accounts = BootAccounts::default();
    accounts.record(&path, ("old", &admin), ("new", &user)).unwrap();
    let probe = |candidate: &BundleInfo| {
        anyhow::ensure!(candidate.admin.is_none(), "admin is offline");
        Ok("new".to_string())
    };
    assert!(accounts.running(&path, &user, probe).is_err());
    std::fs::write(&pid, "11").unwrap();
    assert_eq!(accounts.running(&path, &user, probe).unwrap().admin, None);
}

#[test]
fn recovering_a_pending_user_account_requires_a_new_boot_and_its_exact_generation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("atomics/accounts");
    let pid = dir.path().join("ha.pid");
    std::fs::write(&pid, "10").unwrap();
    let (admin, user) = (bundle(true), bundle(false));
    let mut accounts = BootAccounts::default();
    accounts.record(&path, ("old", &admin), ("old", &admin)).unwrap();
    accounts.remember_pending(&path, "new", &user).unwrap();
    let accounts = BootAccounts::load(&path).unwrap();
    assert!(
        accounts
            .running(&path, &user, |candidate| {
                assert!(candidate.admin.is_some(), "the running boot has not admitted the user route");
                bail!("admin is offline")
            })
            .is_err()
    );
    std::fs::write(pid, "11").unwrap();
    let identify = |system: &str| {
        accounts.running(&path, &user, |candidate| {
            anyhow::ensure!(candidate.admin.is_none(), "admin is offline");
            Ok(system.to_owned())
        })
    };
    assert!(identify("other").is_err());
    assert!(identify("new").unwrap().admin.is_none());

    // Another update of the same running generation may also end before publication.
    let mut retry = BootAccounts::load(&path).unwrap();
    retry.remember_pending(&path, "new", &user).unwrap();
    let recovered = BootAccounts::load(&path).unwrap().running(&path, &user, |candidate| {
        anyhow::ensure!(candidate.admin.is_none(), "admin is offline");
        Ok("new".into())
    });
    assert!(recovered.unwrap().admin.is_none());
}

#[test]
fn an_unverified_legacy_bundle_cannot_select_the_broker_account() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("atomics/accounts");
    let accounts = BootAccounts::load(&path).unwrap();
    let running = accounts
        .running(&path, &bundle(true), |candidate| {
            anyhow::ensure!(candidate.admin.is_none(), "staged administrator is absent");
            Ok("ordinary-generation".into())
        })
        .unwrap();
    assert!(running.admin.is_none());
    for staged in [bundle(false), bundle(true)] {
        assert!(
            BootAccounts::load(&path)
                .unwrap()
                .running(&path, &staged, |_| bail!("running account not established"))
                .is_err()
        );
    }
}
