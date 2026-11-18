use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use atomics_core::usage::Usage;
use serde_json::{Value, json};

pub fn run(socket: PathBuf) {
    if std::env::var_os("ATOMICS_STATUSLINE_FORWARDED").is_some() {
        return;
    }
    let mut input = Vec::new();
    if std::io::stdin().read_to_end(&mut input).is_err() {
        return;
    }
    let Ok(payload) = serde_json::from_slice::<Value>(&input) else { return };
    if let Ok(mut stream) = UnixStream::connect(socket) {
        let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
        if stream.set_read_timeout(Some(Duration::from_secs(1))).is_ok()
            && writeln!(stream, "{}", json!({"statusline": payload})).is_ok()
        {
            // Keep the socket open until ATOMICS has read the report.
            let _ = stream.read(&mut [0]);
        }
    }
    let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| atomics_core::paths::home().join(".claude"));
    if let Some(command) = user_command(&config_dir) {
        if let Ok(mut child) = Command::new("/bin/sh")
            .current_dir(&config_dir)
            .args(["-c", &command])
            .env("ATOMICS_STATUSLINE_FORWARDED", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(&input);
            }
            let _ = child.wait();
        }
        return;
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let usage = Usage::claude(&payload, now);
    let remaining = |window: &Option<atomics_core::usage::Window>| {
        window
            .as_ref()
            .filter(|w| w.resets_at > now)
            .map(|w| format!("{:.0}%", w.left_percent))
            .unwrap_or_else(|| "—".into())
    };
    println!("Claude · session {} · weekly {}", remaining(&usage.session), remaining(&usage.weekly));
}

fn user_command(config_dir: &Path) -> Option<String> {
    let bytes = std::fs::read(config_dir.join("settings.json")).ok()?;
    let settings: Value = serde_json::from_slice(&bytes).ok()?;
    settings["statusLine"]["command"].as_str().map(str::to_owned)
}
