use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use imacs_core::usage::Usage;
use serde_json::{Value, json};

pub fn run(socket: PathBuf) {
    if std::env::var_os("IMACS_STATUSLINE_FORWARDED").is_some() {
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
            // Keep the socket open until IMACS has read the report.
            let _ = stream.read(&mut [0]);
        }
    }
    let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| imacs_core::paths::home().join(".claude"));
    let project = payload["workspace"]["project_dir"].as_str().or_else(|| payload["cwd"].as_str());
    if let Some(command) = user_command(&config_dir, project.map(Path::new)) {
        if let Ok(mut child) = Command::new("sh")
            .args(["-c", &command])
            .env("IMACS_STATUSLINE_FORWARDED", "1")
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
    let remaining = |window: &Option<imacs_core::usage::Window>| {
        window
            .as_ref()
            .filter(|w| w.resets_at > now)
            .map(|w| format!("{:.0}%", w.remaining))
            .unwrap_or_else(|| "—".into())
    };
    println!("Claude · session {} · weekly {}", remaining(&usage.session), remaining(&usage.weekly));
}

fn user_command(config_dir: &Path, project: Option<&Path>) -> Option<String> {
    let mut files = vec![config_dir.join("settings.json")];
    if let Some(project) = project {
        files.extend([project.join(".claude/settings.json"), project.join(".claude/settings.local.json")]);
    }
    let mut command = None;
    for path in files {
        let Some(settings) = std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        else {
            continue;
        };
        if let Some(statusline) = settings.get("statusLine") {
            command = statusline["command"].as_str().map(str::to_owned);
        }
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_statusline_follows_settings_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join(".claude")).unwrap();
        assert!(user_command(dir.path(), Some(&project)).is_none());
        std::fs::write(dir.path().join("settings.json"), r#"{"statusLine":{"command":"user"}}"#).unwrap();
        assert_eq!(user_command(dir.path(), Some(&project)).as_deref(), Some("user"));
        std::fs::write(project.join(".claude/settings.json"), r#"{"statusLine":{"command":"project"}}"#).unwrap();
        std::fs::write(project.join(".claude/settings.local.json"), r#"{"statusLine":{"command":"local"}}"#).unwrap();
        assert_eq!(user_command(dir.path(), Some(&project)).as_deref(), Some("local"));
        std::fs::write(project.join(".claude/settings.local.json"), r#"{"statusLine":null}"#).unwrap();
        assert!(user_command(dir.path(), Some(&project)).is_none());
    }
}
