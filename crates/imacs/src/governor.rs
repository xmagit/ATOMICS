//! The machine-wide limits behind every agent IMACS starts: one jobserver pool that all
//! their compilers share, and slots that whole test runs and QA instances wait for.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use imacs_core::governor::{ResourceConfig, SlotClass, SlotQueue, Ticket};
use serde_json::{Value, json};

/// A GNU make jobserver in a named pipe. cargo and rustc take a token per job, so agents
/// that inherit it share `tokens` jobs between them, however many run.
pub struct Jobserver {
    path: PathBuf,
    /// Held open so the pipe and its tokens live as long as IMACS.
    _pipe: File,
}

impl Jobserver {
    pub fn create(path: &Path, tokens: usize) -> Result<Jobserver> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(path);
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
        // SAFETY: c_path is a valid NUL-terminated path for the duration of the call.
        if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } != 0 {
            return Err(std::io::Error::last_os_error()).context(format!("creating {}", path.display()));
        }
        let mut pipe = OpenOptions::new().read(true).write(true).open(path)?;
        pipe.write_all(&vec![b'+'; tokens])?;
        Ok(Jobserver { path: path.to_path_buf(), _pipe: pipe })
    }

    /// The variables that hand the pool to cargo (CARGO_MAKEFLAGS) and to make or rustc (MAKEFLAGS).
    pub fn env(&self) -> [(String, String); 2] {
        let flags = format!("-j --jobserver-auth=fifo:{}", self.path.display());
        [("MAKEFLAGS".into(), flags.clone()), ("CARGO_MAKEFLAGS".into(), flags)]
    }
}

pub struct Governor {
    queue: Mutex<SlotQueue>,
    changed: Condvar,
    config: Mutex<ResourceConfig>,
    pub cores: usize,
}

impl Governor {
    pub fn new(config: ResourceConfig) -> Governor {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        Governor { queue: Mutex::default(), changed: Condvar::new(), config: Mutex::new(config), cores }
    }

    pub fn set_config(&self, config: ResourceConfig) {
        *self.config.lock().unwrap() = config;
        self.grant();
    }

    pub fn slots(&self, class: SlotClass) -> usize {
        self.config.lock().unwrap().slots(class, self.cores)
    }

    fn grant(&self) {
        let config = self.config.lock().unwrap().clone();
        self.queue.lock().unwrap().grant(|class| config.slots(class, self.cores));
        self.changed.notify_all();
    }

    pub fn snapshot(&self) -> (Vec<Ticket>, Vec<Ticket>) {
        let queue = self.queue.lock().unwrap();
        (queue.running().to_vec(), queue.waiting().cloned().collect())
    }

    /// Serves one `imacs queue` connection: waits for a slot, telling the client its place,
    /// then holds the slot until the client hangs up (its command ended, or it died).
    pub fn serve(&self, stream: &UnixStream, request: &Value) -> Result<()> {
        let class = request.get("class").and_then(Value::as_str).and_then(SlotClass::parse).unwrap_or(SlotClass::Test);
        let label = request.get("label").and_then(Value::as_str).unwrap_or_default().to_string();
        let ancestors = request
            .get("ancestors")
            .and_then(Value::as_array)
            .map(|pids| pids.iter().filter_map(Value::as_u64).map(|pid| pid as u32).collect())
            .unwrap_or_default();
        let id = self.queue.lock().unwrap().enqueue(class, label, ancestors);
        let result = self.wait_and_hold(stream, id);
        self.queue.lock().unwrap().release(id);
        self.grant();
        result
    }

    fn wait_and_hold(&self, stream: &UnixStream, id: u64) -> Result<()> {
        self.grant();
        let mut told = None;
        loop {
            let position = {
                let queue = self.queue.lock().unwrap();
                if queue.is_running(id) {
                    break;
                }
                queue.position(id)
            };
            if position != told {
                send(stream, &json!({ "position": position }))?;
                told = position;
            }
            let queue = self.queue.lock().unwrap();
            drop(self.changed.wait_timeout(queue, Duration::from_millis(500)).unwrap());
            if hung_up(stream)? {
                bail!("client left the queue");
            }
        }
        send(stream, &json!({ "granted": true }))?;
        stream.set_read_timeout(None)?;
        let mut rest = Vec::new();
        let _ = (&mut &*stream).read_to_end(&mut rest);
        Ok(())
    }
}

fn send(stream: &UnixStream, value: &Value) -> Result<()> {
    let mut writer = stream;
    writer.write_all(format!("{value}\n").as_bytes())?;
    Ok(())
}

/// Whether the client closed its end while it waited.
fn hung_up(stream: &UnixStream) -> Result<bool> {
    stream.set_read_timeout(Some(Duration::from_millis(1)))?;
    let mut byte = [0u8; 1];
    Ok(match (&mut &*stream).read(&mut byte) {
        Ok(0) => true,
        Ok(_) => false,
        Err(err) if matches!(err.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => false,
        Err(_) => true,
    })
}

/// `imacs queue`: waits for a slot in the running IMACS, then runs the command and exits
/// with its status. Without a running IMACS the command runs right away.
pub fn run_queued(socket: &Path, class: SlotClass, command: &[String]) -> i32 {
    let Some((program, args)) = command.split_first() else {
        eprintln!("imacs queue: nothing to run");
        return 2;
    };
    let label = command.join(" ");
    let held = match UnixStream::connect(socket) {
        Ok(stream) => match wait_for_slot(stream, class, &label) {
            Ok(stream) => Some(stream),
            Err(err) => {
                eprintln!("imacs queue: {err:#}; running without a slot");
                None
            }
        },
        Err(_) => None,
    };
    let status = std::process::Command::new(program).args(args).status();
    drop(held);
    match status {
        Ok(status) => status.code().unwrap_or_else(|| {
            use std::os::unix::process::ExitStatusExt;
            128 + status.signal().unwrap_or(1)
        }),
        Err(err) => {
            eprintln!("imacs queue: could not run {program}: {err}");
            127
        }
    }
}

fn wait_for_slot(stream: UnixStream, class: SlotClass, label: &str) -> Result<UnixStream> {
    let ancestors = crate::hook_client::ancestors();
    let request = json!({ "cmd": "queue", "class": class.name(), "label": label, "ancestors": ancestors });
    send(&stream, &request)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            bail!("IMACS closed the queue");
        }
        let reply: Value = serde_json::from_str(&line)?;
        if reply.get("granted").is_some() {
            return Ok(stream);
        }
        if let Some(error) = reply.get("error").and_then(Value::as_str) {
            bail!("{error}");
        }
        if let Some(position) = reply.get("position").and_then(Value::as_u64) {
            eprintln!("imacs queue: waiting for a {} slot ({position} in line)", class.name());
        }
    }
}

/// One-minute load average, or None where the platform has none.
pub fn load_average() -> Option<f64> {
    let mut loads = [0f64; 3];
    // SAFETY: loads has room for the 3 values requested.
    (unsafe { libc::getloadavg(loads.as_mut_ptr(), 3) } >= 1).then_some(loads[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;
    use std::time::Instant;

    #[test]
    fn jobserver_pipe_holds_its_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let jobserver = Jobserver::create(&dir.path().join("jobs"), 3).unwrap();
        let mut reader = OpenOptions::new().read(true).write(true).open(dir.path().join("jobs")).unwrap();
        let mut tokens = [0u8; 3];
        reader.read_exact(&mut tokens).unwrap();
        assert_eq!(&tokens, b"+++");
        let [(make, flags), (cargo, _)] = jobserver.env();
        assert_eq!((make.as_str(), cargo.as_str()), ("MAKEFLAGS", "CARGO_MAKEFLAGS"));
        assert!(flags.ends_with(&format!("fifo:{}", dir.path().join("jobs").display())));
    }

    fn client(socket: &Path, class: &str) -> (UnixStream, BufReader<UnixStream>) {
        let stream = UnixStream::connect(socket).unwrap();
        send(&stream, &json!({ "cmd": "queue", "class": class, "label": class, "ancestors": [] })).unwrap();
        let reader = BufReader::new(stream.try_clone().unwrap());
        (stream, reader)
    }

    fn next(reader: &mut BufReader<UnixStream>) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn slots_are_held_until_the_client_hangs_up() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("s");
        let listener = UnixListener::bind(&socket).unwrap();
        let governor = Arc::new(Governor::new(ResourceConfig { test_slots: Some(1), ..ResourceConfig::default() }));
        let server = governor.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let governor = server.clone();
                std::thread::spawn(move || {
                    let mut line = String::new();
                    BufReader::new(&stream).read_line(&mut line).unwrap();
                    let _ = governor.serve(&stream, &serde_json::from_str(&line).unwrap());
                });
            }
        });
        let (first, mut first_reader) = client(&socket, "test");
        assert_eq!(next(&mut first_reader)["granted"], true);
        let (_second, mut second_reader) = client(&socket, "test");
        assert_eq!(next(&mut second_reader)["position"], 1);
        let (gone, mut gone_reader) = client(&socket, "test");
        assert_eq!(next(&mut gone_reader)["position"], 2);
        drop(gone);
        drop(gone_reader);
        let deadline = Instant::now() + Duration::from_secs(5);
        while governor.snapshot().1.len() != 1 {
            assert!(Instant::now() < deadline, "a waiter that hung up must leave the queue");
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(first);
        drop(first_reader);
        assert_eq!(next(&mut second_reader)["granted"], true);
        let (running, waiting) = governor.snapshot();
        assert_eq!((running.len(), waiting.len()), (1, 0));
    }

    #[test]
    fn the_command_runs_with_its_exit_status_without_a_running_imacs() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.sock");
        assert_eq!(run_queued(&missing, SlotClass::Test, &["sh".into(), "-c".into(), "exit 7".into()]), 7);
        assert_eq!(run_queued(&missing, SlotClass::Test, &[]), 2);
    }

    #[test]
    fn load_average_is_available() {
        assert!(load_average().is_some_and(|load| load >= 0.0));
    }
}
