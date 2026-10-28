//! `imacs hook`: forwards lifecycle events and permission replies. Failures return
//! no decision so the agent can use its native approval prompt.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use imacs_core::hook::{HookReply, HookRequest};

pub fn run(socket: Option<std::path::PathBuf>, pane: Option<String>) {
    // A globally installed Claude hook must not attach this pane to the default instance.
    if socket.is_none() && std::env::var("TERM_PROGRAM").as_deref() == Ok("imacs") {
        return;
    }
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    let Ok(payload) = serde_json::from_str(&input) else { return };
    let ancestors = if pane.is_none() {
        std::process::Command::new("/bin/ps")
            .args(["-axo", "pid=,ppid="])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| imacs_core::hook::ancestor_pids(std::process::id(), &String::from_utf8_lossy(&output.stdout)))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let request = HookRequest { pane, ancestors, payload };
    if let Some(stdout) = forward(&request, socket.unwrap_or_else(imacs_core::paths::socket_path)) {
        print!("{stdout}");
        let _ = std::io::stdout().flush();
    }
}

fn forward(request: &HookRequest, socket: std::path::PathBuf) -> Option<String> {
    let stream = UnixStream::connect(socket).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(1))).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(12))).ok()?;
    let mut writer = &stream;
    writer.write_all(serde_json::to_string(request).ok()?.as_bytes()).ok()?;
    writer.write_all(b"\n").ok()?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).ok()?;
    let reply: HookReply = serde_json::from_str(&line).ok()?;
    reply.stdout
}
