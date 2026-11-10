//! Native clipboard path. No value, copy command or authentication result crosses the protocol.
use anyhow::{Result, bail};
use std::path::Path;

pub const POLICY: &str = include_str!("../../../assets/com.atomics.copy-secret.policy");
const POLICY_FILE: &str = "com.atomics.copy-secret.policy";
const ENABLE: &str =
    "Run sudo atomics install --secret-copy-policy; on NixOS enable programs.atomics and run sudo nixos-rebuild switch";

pub fn unavailable() -> Option<String> {
    if cfg!(target_os = "linux") {
        let present = ["/usr/share/polkit-1/actions", "/run/current-system/sw/share/polkit-1/actions"]
            .iter()
            .any(|dir| std::fs::read_to_string(Path::new(dir).join(POLICY_FILE)).is_ok_and(|text| text == POLICY));
        if !present {
            return Some(format!("Copy unavailable: authentication policy is missing. {ENABLE}."));
        }
        if crate::setup::find_program("pkcheck").is_none() {
            return Some("Copy unavailable: install polkit (pkcheck) and a desktop polkit authentication agent, then reopen ATOMICS.".into());
        }
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none()
            || (std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none())
        {
            return Some("Copy unavailable: no local desktop authentication session. Open ATOMICS in a desktop login with a running polkit authentication agent; a headless or remote window cannot copy.".into());
        }
    }
    None
}

pub fn install_policy() -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("The Copy policy is needed only on Linux");
    }
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        bail!("Installing the Copy policy requires root. {ENABLE}.");
    }
    let dir = Path::new("/usr/share/polkit-1/actions");
    std::fs::create_dir_all(dir)?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    file.write_all(POLICY.as_bytes())?;
    file.as_file().set_permissions(std::fs::Permissions::from_mode(0o644))?;
    file.persist(dir.join(POLICY_FILE)).map_err(|_| anyhow::anyhow!("Could not install Copy policy"))?;
    Ok(())
}

pub fn copy(keychain: crate::relay::Keychain, name: &str) -> Result<()> {
    if let Some(reason) = unavailable() {
        bail!("{reason}");
    }
    authenticated_copy(keychain, name, authenticate, write_clipboard)
}

fn authenticated_copy(
    keychain: crate::relay::Keychain,
    name: &str,
    auth: impl FnOnce() -> Result<()>,
    write: impl FnOnce(&str) -> Result<()>,
) -> Result<()> {
    auth()?;
    // Read only after fresh authentication. The value goes straight to the native clipboard,
    // never into egui's output, the host's model, a reply, a toast or an edit field.
    let store = crate::credentials::Store::new(keychain);
    let Some((_, value)) = store.secret_values()?.into_iter().find(|(spec, _)| spec.name == name) else {
        bail!("Secret no longer exists");
    };
    write(&value)
}

fn write_clipboard(value: &str) -> Result<()> {
    static CLIPBOARD: std::sync::OnceLock<std::sync::Mutex<Option<arboard::Clipboard>>> = std::sync::OnceLock::new();
    let mut clipboard =
        CLIPBOARD.get_or_init(Default::default).lock().map_err(|_| anyhow::anyhow!("Clipboard unavailable"))?;
    if clipboard.is_none() {
        *clipboard = Some(arboard::Clipboard::new().map_err(|_| anyhow::anyhow!("Clipboard unavailable"))?);
    }
    clipboard.as_mut().unwrap().set_text(value.to_string()).map_err(|_| anyhow::anyhow!("Clipboard unavailable"))?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn authenticate() -> Result<()> {
    let program = crate::setup::find_program("pkcheck")
        .ok_or_else(|| anyhow::anyhow!("Copy requires polkit and a desktop authentication agent"))?;
    // Include process start time and uid so a reused PID cannot authorize another process.
    let stat = std::fs::read_to_string("/proc/self/stat")?;
    let start = stat
        .rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(19))
        .ok_or_else(|| anyhow::anyhow!("Cannot identify the window process"))?;
    let subject = format!("{},{start},{}", std::process::id(), unsafe { libc::getuid() });
    let mut command = std::process::Command::new(program);
    command.args(["--action-id", "com.atomics.copy-secret", "--process", &subject, "--allow-user-interaction"]);
    let output = crate::sandbox::setup::private_output(&mut command, std::time::Duration::from_secs(120));
    if !output.is_ok_and(|out| out.status.success()) {
        bail!(
            "Copy cancelled or OS authentication unavailable. Start a desktop polkit authentication agent and retry; no value was copied."
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn authenticate() -> Result<()> {
    use objc2::{
        class, msg_send,
        rc::Retained,
        runtime::{AnyObject, Bool},
    };
    #[link(name = "LocalAuthentication", kind = "framework")]
    unsafe extern "C" {}
    let (tx, rx) = std::sync::mpsc::channel();
    // SAFETY: documented LAContext/NSString selectors and ABI. A new context and a zero
    // reuse interval require a fresh OS prompt. The retained context lives through the reply.
    unsafe {
        let context: Retained<AnyObject> = msg_send![class!(LAContext), new];
        let _: () = msg_send![&*context, setTouchIDAuthenticationAllowableReuseDuration: 0.0f64];
        let text = "Copy a stored ATOMICS secret to the clipboard";
        let reason: objc2::rc::Allocated<AnyObject> = msg_send![class!(NSString), alloc];
        let reason: Retained<AnyObject> =
            msg_send![reason, initWithBytes: text.as_ptr(), length: text.len(), encoding: 4usize];
        let reply = block2::RcBlock::new(move |success: Bool, _error: *mut AnyObject| {
            let _ = tx.send(success.as_bool());
        });
        let _: () = msg_send![&*context, evaluatePolicy: 2isize, localizedReason: &*reason, reply: &*reply];
        let approved = rx.recv_timeout(std::time::Duration::from_secs(120)).unwrap_or(false);
        let _: () = msg_send![&*context, invalidate];
        if !approved {
            bail!("Copy cancelled or LocalAuthentication unavailable; no value was copied");
        }
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn authenticate() -> Result<()> {
    bail!("Copy requires OS authentication on macOS or Linux")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_copy_reads_only_after_authentication_and_never_on_cancellation() {
        let (dir, keychain, store) = crate::credentials::tests::machine(false);
        store
            .add_secret(
                atomics_core::secrets::Secret { name: "EXAMPLE_KEY".into(), ..Default::default() },
                "canary-copy-private",
            )
            .unwrap();
        let before = std::fs::read(dir.path().join("keychain-argv")).unwrap();
        let denied = authenticated_copy(
            keychain.clone(),
            "EXAMPLE_KEY",
            || bail!("OS authentication cancelled"),
            |_| panic!("copied without authentication"),
        );
        assert!(denied.is_err());
        assert_eq!(std::fs::read(dir.path().join("keychain-argv")).unwrap(), before);
        let mut copied = false;
        authenticated_copy(
            keychain,
            "EXAMPLE_KEY",
            || Ok(()),
            |value| {
                copied = value == "canary-copy-private";
                Ok(())
            },
        )
        .unwrap();
        assert!(copied);
        assert!(!POLICY.contains("_keep"));
        assert!(POLICY.contains("<allow_active>auth_self</allow_active>"));
    }
}
