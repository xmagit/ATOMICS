//! `cox hook`: run by Claude Code for every subscribed event. It must never slow
//! down or break an agent, so every failure is silent and it always exits 0.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use cox_core::hook::{HookReply, HookRequest};

pub fn run() {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    let Ok(payload) = serde_json::from_str(&input) else { return };
    let request = HookRequest { pane: std::env::var("COX_PANE").ok(), payload };
    if let Some(stdout) = forward(&request) {
        print!("{stdout}");
        let _ = std::io::stdout().flush();
    }
}

fn forward(request: &HookRequest) -> Option<String> {
    let stream = UnixStream::connect(cox_core::paths::socket_path()).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(1))).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    let mut writer = &stream;
    writer.write_all(serde_json::to_string(request).ok()?.as_bytes()).ok()?;
    writer.write_all(b"\n").ok()?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).ok()?;
    let reply: HookReply = serde_json::from_str(&line).ok()?;
    reply.stdout
}
