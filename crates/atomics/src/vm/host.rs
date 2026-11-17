//! `atomics vm restart-host`: restarts the ATOMICS host in this Mac's VM from outside it, for a host the window
//! cannot reach (a lockout, a hang) and that refuses `ctl restart` because a service runs it. It goes the way
//! `atomics vm update` changes the system: `systemctl restart atomics-host` as the admin user over Lima's ssh
//! config in hardened mode, as the VM's user with sudo otherwise. Like an update, it asks the host first what
//! work is busy and stops there unless told to go on (`--yes`); a host that does not answer counts as busy,
//! since nothing then says what it runs. The sessions the host ran resume when it starts again.

use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::BundleInfo;

/// What the admin runs. systemd stops the host as a service stop does, so it keeps a record of what ran.
pub(crate) const RESTART: &str = "sudo systemctl restart atomics-host";

/// How long the host may take to answer what it runs before it counts as stuck.
const LIST_TIMEOUT_SECS: u32 = 20;

/// The restart was refused for busy work, listed; `--yes` goes on anyway. `atomics vm restart-host` exits with
/// [`Busy::EXIT`] for it, so the window can ask.
#[derive(Debug)]
pub struct Busy(pub Vec<String>);

impl Busy {
    pub const EXIT: i32 = 3;
}

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "work in the VM is busy:\n  {}\nnot restarted; wait for it, or use --yes to restart anyway (its sessions \
             resume, and what they were running ends)",
            self.0.join("\n  ")
        )
    }
}

impl std::error::Error for Busy {}

pub fn restart_host(yes: bool) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!(super::MACOS_ONLY);
    }
    let info =
        BundleInfo::read(&super::installed_dir()).context("no readable VM bundle; run atomics vm setup first")?;
    if info.host.is_none() {
        bail!("the VM's bundle has no ATOMICS host to restart: the window runs the agents on this Mac");
    }
    let state = super::lima_status_for_command(&info.instance)?
        .context("the Lima instance is missing; run atomics vm setup")?;
    if state != "Running" {
        bail!("VM {} is {state}: its host is not running; start the VM with atomics vm start", info.instance);
    }
    let list = format!(r#"exec timeout {LIST_TIMEOUT_SECS} atomics ctl '{{"cmd":"list"}}'"#);
    let answer = super::update::guest(&info, &list).and_then(|answer| Ok(serde_json::from_str(&answer)?));
    restart(&info, yes, answer, super::system_command(&info, RESTART)?)?;
    println!("The ATOMICS host in VM {} restarted; the sessions it ran resume.", info.instance);
    Ok(())
}

/// Restarts with `command` unless `answer`, the host's `list`, shows busy work and `yes` is not given.
pub(crate) fn restart(info: &BundleInfo, yes: bool, answer: Result<Value>, mut command: Command) -> Result<()> {
    let blockers = match answer.and_then(|answer| super::update::busy(&answer)) {
        Ok(blockers) => blockers,
        Err(error) => vec![format!("the host did not say what it runs ({error:#}); it may be stuck")],
    };
    if !blockers.is_empty() {
        if !yes {
            return Err(Busy(blockers).into());
        }
        println!("Restarting anyway (--yes):\n  {}", blockers.join("\n  "));
    }
    let status = command.stdin(Stdio::null()).status().context("running the restart in the VM")?;
    if !status.success() {
        bail!("restarting the host in VM {} failed ({status}); see the output above", info.instance);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hardened() -> BundleInfo {
        serde_json::from_value(json!({
            "version": 1, "instance": "atomics-test", "user": "atomics", "home": "/home/atomics",
            "macUser": "alice", "installDir": "/Users/alice/.local/share/atomics-vm", "host": {"port": 47800},
            "admin": "admin"
        }))
        .unwrap()
    }

    fn idle() -> Result<Value> {
        Ok(json!({"ok": true, "result": {"sessions": [], "update_blockers": []}}))
    }

    fn busy() -> Result<Value> {
        Ok(json!({"ok": true, "result": {"sessions": [], "update_blockers": ["agent parser (s1) is working"]}}))
    }

    /// The restart reaches the host as the admin, over its own ssh connection with Lima's config, and runs
    /// systemctl there; busy work, or a host that does not answer, stops it unless the user says to go on.
    #[test]
    fn the_restart_goes_through_the_admin_and_asks_before_stopping_busy_work() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let log = dir.path().join("ssh.log");
        crate::test_support::write_program(bin.join("ssh"), "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$SSH_LOG\"\n");
        let command = || {
            let mut command = crate::vm::system_command(&hardened(), RESTART).unwrap();
            command.env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap())).env("SSH_LOG", &log);
            command
        };
        let ran = || std::fs::read_to_string(&log).unwrap_or_default();

        let refused = restart(&hardened(), false, busy(), command()).unwrap_err();
        let Some(Busy(blockers)) = refused.downcast_ref::<Busy>() else { panic!("{refused:#}") };
        assert_eq!(blockers, &["agent parser (s1) is working"]);
        let stuck = restart(&hardened(), false, Err(anyhow::anyhow!("timed out")), command()).unwrap_err();
        assert!(stuck.downcast_ref::<Busy>().unwrap().0[0].contains("may be stuck"), "{stuck:#}");
        assert_eq!(ran(), "", "nothing is restarted before the user says so");

        restart(&hardened(), false, idle(), command()).unwrap();
        let args = ran();
        let args: Vec<_> = args.lines().collect();
        assert!(args.windows(2).any(|pair| pair == ["-l", "admin"]), "as the admin: {args:?}");
        assert!(args.contains(&"ControlMaster=no") && args.contains(&"ControlPath=none"), "{args:?}");
        assert!(args.windows(2).any(|pair| pair[0] == "-F" && pair[1].ends_with("atomics-test/ssh.config")));
        assert_eq!(args.last().unwrap(), &format!("bash --login -c '{RESTART}'"));

        std::fs::remove_file(&log).unwrap();
        restart(&hardened(), true, busy(), command()).unwrap();
        assert!(ran().contains(RESTART), "--yes restarts over busy work");
    }
}
