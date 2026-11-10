//! A checkout as the source of new versions: how the people building ATOMICS update their own machine. What the
//! remote's main holds is built here and run, with no signature behind it, so `[update] source` counts as a setting
//! that runs commands. Nothing moves the checkout while it has changes of its own, and no commit is ever dropped.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use atomics_core::git;
use atomics_core::governor::ResourceConfig;

use super::Found;

const REMOTE_MAIN: &str = "origin/main";
const LOW_PRIORITY: [&str; 3] = ["nice", "-n", "19"];

#[derive(Clone)]
pub struct Source {
    checkout: PathBuf,
    /// What runs the install script in the build queue; nothing in a test, which has no queue to wait in.
    queue: Vec<OsString>,
    /// `[resources] jobs` here, for a build with no ATOMICS whose job pool it could share.
    jobs: usize,
    /// The script's output, for when it fails.
    log: PathBuf,
    /// The running build, which a check compares the checkout with.
    version: String,
}

impl Source {
    pub fn new(path: &str, resources: &ResourceConfig) -> Source {
        let program = crate::setup::executable_path().map_or_else(|_| "atomics".into(), PathBuf::into_os_string);
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        Source {
            checkout: atomics_core::paths::expand(path),
            queue: vec![program, "queue".into(), "--class".into(), "test".into(), "--".into()],
            jobs: resources.jobs(cores),
            log: atomics_core::paths::atomics_home().join("update.log"),
            version: crate::restart::version().into(),
        }
    }

    fn git(&self, args: &[&str]) -> Result<String> {
        git::git(&self.checkout, args).map(|text| text.trim().to_string())
    }

    fn fetch(&self) -> Result<()> {
        self.git(&["fetch", "--quiet", "origin", "+refs/heads/main:refs/remotes/origin/main"])
            .map(|_| ())
            .with_context(|| format!("Could not fetch {REMOTE_MAIN} in {}", self.checkout.display()))
    }

    /// Newer when the checkout is behind its remote's main, or holds commits this build lacks, such as after a
    /// build that failed once the checkout had moved.
    pub fn check(&self) -> Result<Option<Found>> {
        self.fetch()?;
        let behind = commit_count(&self.git(&["rev-list", "--count", &format!("HEAD..{REMOTE_MAIN}")])?)?;
        if behind == 0 && !self.build_lacks_checkout() {
            return Ok(None);
        }
        let detail = if behind > 0 {
            let subject = self.git(&["log", "-1", "--format=%s", REMOTE_MAIN])?;
            let commits = if behind == 1 { "new commit" } else { "new commits" };
            format!("{behind} {commits} on {REMOTE_MAIN}; the latest: {subject}")
        } else {
            format!("This build lacks commits {} already has.", self.checkout.display())
        };
        let version = self.git(&["rev-parse", "--short", REMOTE_MAIN])?;
        Ok(Some(Found { version, detail: Some(detail), notes: None, assets: None }))
    }

    /// A build's version is git's own description of its commit, which git reads back; a build from elsewhere names
    /// nothing here.
    fn build_lacks_checkout(&self) -> bool {
        let build = format!("{}^{{commit}}", self.version.trim_end_matches("-dirty"));
        let Ok(build) = self.git(&["rev-parse", "--verify", "--quiet", &build]) else { return false };
        matches!(git::is_ancestor(&self.checkout, "HEAD", &build), Ok(Some(false)))
    }

    /// Why this install cannot build from a checkout. On Linux the install script always installs to one place, and a
    /// restart reruns the program running now: from anywhere else that would rerun the old build, and offer the same
    /// update again, a full build each time.
    pub fn unavailable(&self) -> Option<String> {
        if cfg!(target_os = "macos") {
            return None;
        }
        let running = crate::setup::executable_path().ok()?;
        let installs_to = atomics_core::paths::home().join(".local/bin/atomics");
        let resolved = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        (resolved(&running) != resolved(&installs_to)).then(|| {
            format!(
                "Building from {} installs to {}, but this runs from {}. Start that one to update from a checkout.",
                self.checkout.display(),
                installs_to.display(),
                running.display()
            )
        })
    }

    pub fn install(&self, progress: &dyn Fn(String)) -> Result<()> {
        progress(format!("Fetching {REMOTE_MAIN}"));
        let changes = self.git(&["status", "--porcelain", "--untracked-files=no"])?;
        if !changes.is_empty() {
            bail!(
                "{} has uncommitted changes, so it stays where it is. Commit or stash them, then update again.",
                self.checkout.display()
            );
        }
        self.fetch()?;
        self.move_to_remote_main()?;
        progress(format!("Building in the queue, at low priority (log: {})", self.log.display()));
        self.run_install_script()
    }

    /// Fast-forwards `main` where it is checked out and behind. Anywhere else the checkout detaches, which leaves
    /// its branch as it was; a commit only HEAD holds would be lost, so that is refused.
    fn move_to_remote_main(&self) -> Result<()> {
        if self.git(&["rev-parse", "HEAD"])? == self.git(&["rev-parse", REMOTE_MAIN])? {
            return Ok(());
        }
        let branch = self.git(&["symbolic-ref", "--quiet", "--short", "HEAD"]).ok();
        let moved = if branch.as_deref() == Some("main")
            && matches!(git::is_ancestor(&self.checkout, "HEAD", REMOTE_MAIN), Ok(Some(true)))
        {
            self.git(&["merge", "--quiet", "--ff-only", REMOTE_MAIN])
        } else {
            let only_here = self.git(&["rev-list", "-1", "HEAD", "--not", "--branches", "--remotes", "--tags"])?;
            if branch.is_none() && !only_here.is_empty() {
                bail!(
                    "{} is on commits no branch holds, so it stays where it is. Put them on a branch, then update again.",
                    self.checkout.display()
                );
            }
            self.git(&["switch", "--quiet", "--detach", REMOTE_MAIN])
        };
        moved.map(|_| ()).with_context(|| format!("Could not move {} to {REMOTE_MAIN}", self.checkout.display()))
    }

    fn run_install_script(&self) -> Result<()> {
        let (script, args) = install_script(cfg!(target_os = "macos"));
        let script = self.checkout.join(script);
        if !script.is_file() {
            bail!("{} has no install script at {}.", self.checkout.display(), script.display());
        }
        let log =
            std::fs::File::create(&self.log).with_context(|| format!("Could not write {}", self.log.display()))?;
        let argv = command_line(&self.queue, &script, &args);
        let pin = atomics_core::governor::pinned_rust_toolchain(&self.checkout);
        let home =
            std::env::var_os("RUSTUP_HOME").map_or_else(|| atomics_core::paths::home().join(".rustup"), PathBuf::from);
        let path = std::env::var_os("PATH").unwrap_or_default();
        let pool = crate::governor::Jobserver::path_for(&atomics_core::paths::atomics_home());
        let status = Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&self.checkout)
            // A job pool of the shell this came from is not this build's.
            .env_remove("CARGO_MAKEFLAGS")
            .env_remove("MAKEFLAGS")
            .env_remove("MFLAGS")
            .envs(toolchain_env(pin.as_deref(), &home, env!("ATOMICS_TARGET"), &path))
            .envs(jobs_env(
                pool.exists().then_some(pool.as_path()),
                self.jobs,
                std::env::var_os("CARGO_BUILD_JOBS").is_some(),
            ))
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .status()
            .with_context(|| format!("Could not start {}", script.display()))?;
        if status.success() {
            return Ok(());
        }
        let output = std::fs::read(&self.log).unwrap_or_default();
        let output = String::from_utf8_lossy(&output);
        let last = output.lines().rev().map(str::trim).find(|line| !line.is_empty()).unwrap_or("it printed nothing");
        Err(anyhow!(
            "The install script stopped ({status}): {last}. It may already have put the new program in place. The whole log \
             is at {}.",
            self.log.display()
        ))
    }
}

fn commit_count(output: &str) -> Result<u64> {
    output.trim().parse().context("Git rev-list returned an invalid commit count; update status is unknown")
}

/// The script that installs from a checkout on this platform, and what it takes. On a Mac it replaces the bundle
/// running now, wherever that is.
fn install_script(macos: bool) -> (&'static str, Vec<OsString>) {
    if !macos {
        return ("scripts/linux-install.sh", vec!["--no-setup".into()]);
    }
    let folder = match super::InstallTarget::current() {
        Ok(super::InstallTarget::Bundle(bundle)) => bundle.parent().map(|parent| parent.as_os_str().to_owned()),
        _ => None,
    };
    ("scripts/macos-app.sh", folder.into_iter().collect())
}

/// The build queue, then low priority, then the script.
fn command_line(queue: &[OsString], script: &Path, args: &[OsString]) -> Vec<OsString> {
    let mut argv = queue.to_vec();
    argv.extend(LOW_PRIORITY.iter().map(OsString::from));
    argv.push("sh".into());
    argv.push(script.as_os_str().to_owned());
    argv.extend_from_slice(args);
    argv
}

/// The toolchain the checkout pins, as rustup takes it and, ahead of a `cargo` from anywhere else, on the PATH.
fn toolchain_env(pin: Option<&str>, rustup_home: &Path, platform: &str, path: &OsStr) -> Vec<(OsString, OsString)> {
    let Some(pin) = pin else { return Vec::new() };
    let mut env = vec![(OsString::from("RUSTUP_TOOLCHAIN"), OsString::from(pin))];
    let bin = rustup_home.join("toolchains").join(format!("{pin}-{platform}")).join("bin");
    if bin.is_dir() {
        let folders = std::iter::once(bin).chain(std::env::split_paths(path));
        if let Ok(joined) = std::env::join_paths(folders) {
            env.push(("PATH".into(), joined));
        }
    }
    env
}

/// The running ATOMICS's job pool when there is one, which agents' builds share; else `[resources] jobs`, unless the
/// caller set its own.
fn jobs_env(pool: Option<&Path>, jobs: usize, caller_set: bool) -> Vec<(OsString, OsString)> {
    if let Some(pool) = pool {
        let flags = format!("-j --jobserver-auth=fifo:{}", pool.display());
        return vec![("CARGO_MAKEFLAGS".into(), flags.into())];
    }
    if jobs == 0 || caller_set {
        return Vec::new();
    }
    vec![("CARGO_BUILD_JOBS".into(), jobs.to_string().into())]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn audit_p7_bad_commit_counts_are_errors_instead_of_up_to_date() {
        for output in ["", "not a count", "-1", "1 2", "18446744073709551616"] {
            let error = commit_count(output).unwrap_err();
            assert!(error.to_string().contains("update status is unknown"), "{error}");
        }
        assert_eq!(commit_count("0\n").unwrap(), 0);
        assert_eq!(commit_count(" 12\n").unwrap(), 12);
    }

    fn run(dir: &Path, args: &[&str]) -> String {
        let output = crate::test_support::output(
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
                .args(args),
        );
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A checkout on `main` of a remote, with an install script that records how it ran, and a second clone to push
    /// new commits from.
    struct Fixture {
        dir: crate::test_support::TempDir,
        source: Source,
    }

    impl Fixture {
        fn new(script: &str) -> Fixture {
            let dir = crate::test_support::tempdir().unwrap();
            let (origin, checkout, other) =
                (dir.path().join("origin.git"), dir.path().join("checkout"), dir.path().join("other"));
            run(dir.path(), &["init", "--quiet", "--bare", "-b", "main", origin.to_str().unwrap()]);
            run(dir.path(), &["clone", "--quiet", origin.to_str().unwrap(), other.to_str().unwrap()]);
            run(&other, &["symbolic-ref", "HEAD", "refs/heads/main"]);
            std::fs::create_dir_all(other.join("scripts")).unwrap();
            crate::test_support::write_program(other.join("scripts/linux-install.sh"), script);
            run(&other, &["add", "."]);
            run(&other, &["commit", "--quiet", "-m", "First"]);
            run(&other, &["push", "--quiet", "origin", "main"]);
            run(dir.path(), &["clone", "--quiet", origin.to_str().unwrap(), checkout.to_str().unwrap()]);
            let source = Source {
                checkout,
                queue: Vec::new(),
                jobs: 2,
                log: dir.path().join("update.log"),
                version: "a build from elsewhere".into(),
            };
            Fixture { dir, source }
        }

        fn other(&self) -> PathBuf {
            self.dir.path().join("other")
        }

        fn push_commit(&self, subject: &str) -> String {
            let other = self.other();
            std::fs::write(other.join("new.txt"), subject).unwrap();
            run(&other, &["add", "."]);
            run(&other, &["commit", "--quiet", "-m", subject]);
            run(&other, &["push", "--quiet", "origin", "main"]);
            run(&other, &["rev-parse", "HEAD"])
        }

        fn head(&self) -> String {
            run(&self.source.checkout, &["rev-parse", "HEAD"])
        }
    }

    const RECORDS: &str = "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$(dirname \"$0\")/../ran\"\n";

    #[test]
    fn a_check_compares_the_checkout_with_origin_main() {
        let fixture = Fixture::new(RECORDS);
        assert_eq!(fixture.source.check().unwrap(), None, "up to date");

        let new = fixture.push_commit("Add the thing");
        let found = fixture.source.check().unwrap().expect("behind by one");
        assert!(new.starts_with(&found.version), "{found:?}");
        assert_eq!(found.detail.as_deref(), Some("1 new commit on origin/main; the latest: Add the thing"));
        assert_ne!(fixture.head(), new, "a check moves nothing");
    }

    #[test]
    fn a_check_says_so_when_a_failed_build_left_the_checkout_ahead_of_this_build() {
        let mut fixture = Fixture::new(RECORDS);
        fixture.source.version = fixture.head();
        assert_eq!(fixture.source.check().unwrap(), None, "the build is what the checkout holds");

        let new = fixture.push_commit("Add the thing");
        run(&fixture.source.checkout, &["pull", "--quiet", "origin", "main"]);
        assert_eq!(fixture.head(), new);
        let found = fixture.source.check().unwrap().expect("the build lacks the checkout's commit");
        assert!(found.detail.unwrap().contains("lacks commits"));
    }

    #[test]
    fn an_update_moves_main_to_origin_main_and_runs_the_install_script() {
        let fixture = Fixture::new(RECORDS);
        let new = fixture.push_commit("Add the thing");
        let steps = std::cell::RefCell::new(Vec::new());
        fixture.source.install(&|step| steps.borrow_mut().push(step)).unwrap();
        assert_eq!(fixture.head(), new);
        assert_eq!(run(&fixture.source.checkout, &["symbolic-ref", "--short", "HEAD"]), "main", "it stays on main");
        let ran = std::fs::read_to_string(fixture.source.checkout.join("ran")).unwrap();
        assert_eq!(ran.trim(), "--no-setup", "nobody is at a terminal to answer setup");
        assert!(steps.borrow().iter().any(|step| step.contains("low priority")), "{steps:?}");
    }

    #[test]
    fn another_branch_stays_as_it_was_while_the_checkout_detaches() {
        let fixture = Fixture::new(RECORDS);
        let checkout = &fixture.source.checkout;
        run(checkout, &["switch", "--quiet", "-c", "feature"]);
        std::fs::write(checkout.join("feature.txt"), "mine").unwrap();
        run(checkout, &["add", "."]);
        run(checkout, &["commit", "--quiet", "-m", "Mine"]);
        let mine = fixture.head();
        let new = fixture.push_commit("Add the thing");
        fixture.source.install(&|_| {}).unwrap();
        assert_eq!(fixture.head(), new);
        assert_eq!(run(checkout, &["rev-parse", "feature"]), mine, "the branch keeps its commit");
    }

    #[test]
    fn uncommitted_changes_and_commits_on_no_branch_stop_the_update_and_move_nothing() {
        let fixture = Fixture::new(RECORDS);
        let checkout = &fixture.source.checkout;
        fixture.push_commit("Add the thing");
        let before = fixture.head();
        std::fs::write(checkout.join("scripts/linux-install.sh"), "#!/bin/sh\nexit 1\n").unwrap();
        let error = format!("{:#}", fixture.source.install(&|_| {}).unwrap_err());
        assert!(error.contains("uncommitted changes"), "{error}");
        assert_eq!(fixture.head(), before);
        run(checkout, &["checkout", "--quiet", "--", "."]);

        run(checkout, &["switch", "--quiet", "--detach"]);
        std::fs::write(checkout.join("lost.txt"), "only here").unwrap();
        run(checkout, &["add", "."]);
        run(checkout, &["commit", "--quiet", "-m", "Only here"]);
        let only_here = fixture.head();
        let error = format!("{:#}", fixture.source.install(&|_| {}).unwrap_err());
        assert!(error.contains("no branch holds"), "{error}");
        assert_eq!(fixture.head(), only_here);
        assert!(!checkout.join("ran").exists(), "no build ran");
    }

    #[test]
    fn a_failed_build_says_why_and_a_missing_script_says_so() {
        let fixture = Fixture::new("#!/bin/sh\necho compiling\necho 'error: it does not build'\nexit 3\n");
        fixture.push_commit("Add the thing");
        let error = format!("{:#}", fixture.source.install(&|_| {}).unwrap_err());
        assert!(error.contains("error: it does not build"), "{error}");
        assert!(error.contains("may already have put the new program in place"), "{error}");
        assert!(error.contains("update.log"), "{error}");

        run(&fixture.other(), &["rm", "--quiet", "scripts/linux-install.sh"]);
        run(&fixture.other(), &["commit", "--quiet", "-m", "No script"]);
        run(&fixture.other(), &["push", "--quiet", "origin", "main"]);
        let error = format!("{:#}", fixture.source.install(&|_| {}).unwrap_err());
        assert!(error.contains("has no install script"), "{error}");
    }

    #[test]
    fn the_build_runs_in_the_queue_at_low_priority_with_the_pinned_toolchain() {
        let queue: Vec<OsString> = ["atomics", "queue", "--class", "test", "--"].map(OsString::from).into();
        let argv = command_line(&queue, Path::new("/co/scripts/linux-install.sh"), &["--no-setup".into()]);
        let expected = [
            "atomics",
            "queue",
            "--class",
            "test",
            "--",
            "nice",
            "-n",
            "19",
            "sh",
            "/co/scripts/linux-install.sh",
            "--no-setup",
        ];
        assert_eq!(argv, expected.map(OsString::from));

        let dir = crate::test_support::tempdir().unwrap();
        let bin = dir.path().join("toolchains/1.99.0-test-platform/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let env = toolchain_env(Some("1.99.0"), dir.path(), "test-platform", OsStr::new("/usr/bin:/bin"));
        assert_eq!(env[0], ("RUSTUP_TOOLCHAIN".into(), "1.99.0".into()));
        let path = std::env::split_paths(&env[1].1).collect::<Vec<_>>();
        assert_eq!(path, [bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")], "ahead of a system cargo");
        assert_eq!(toolchain_env(Some("nightly"), dir.path(), "test-platform", OsStr::new("/bin")).len(), 1);
        assert!(toolchain_env(None, dir.path(), "test-platform", OsStr::new("/bin")).is_empty());

        assert_eq!(
            jobs_env(Some(Path::new("/run/pool.fifo")), 8, true),
            [("CARGO_MAKEFLAGS".into(), "-j --jobserver-auth=fifo:/run/pool.fifo".into())]
        );
        assert_eq!(jobs_env(None, 8, false), [("CARGO_BUILD_JOBS".into(), "8".into())]);
        assert!(jobs_env(None, 8, true).is_empty(), "the caller's own limit stands");
        assert!(jobs_env(None, 0, false).is_empty());
    }

    #[test]
    fn the_pin_is_read_as_rustup_reads_it() {
        use atomics_core::governor::pinned_rust_toolchain as pin;
        let dir = crate::test_support::tempdir().unwrap();
        assert_eq!(pin(dir.path()), None);
        std::fs::write(dir.path().join("rust-toolchain"), "1.98.0\n").unwrap();
        assert_eq!(pin(dir.path()).as_deref(), Some("1.98.0"));
        std::fs::write(dir.path().join("rust-toolchain"), "[toolchain]\nchannel = \"1.97.0\"\n").unwrap();
        assert_eq!(pin(dir.path()).as_deref(), Some("1.97.0"));
        std::fs::write(dir.path().join("rust-toolchain.toml"), "[toolchain]\nchannel = \"1.99.0\"\n").unwrap();
        assert_eq!(pin(dir.path()).as_deref(), Some("1.99.0"));
    }
}
