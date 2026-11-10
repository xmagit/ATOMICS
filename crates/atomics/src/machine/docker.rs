use atomics_core::machine::{Container, MAX_CONTAINERS, Port};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub(super) struct Docker {
    pub timed_out: bool,
    stats: HashMap<String, Stats>,
    checked: Option<Instant>,
    /// How long `docker ps` and `docker stats` may take. The collector runs on its own worker, so a busy
    /// daemon is waited for rather than shown as unavailable.
    ps_timeout: Duration,
    stats_timeout: Duration,
}

impl Default for Docker {
    fn default() -> Self {
        Self {
            timed_out: false,
            stats: HashMap::new(),
            checked: None,
            ps_timeout: Duration::from_secs(3),
            stats_timeout: Duration::from_secs(5),
        }
    }
}

impl Docker {
    pub fn collect(&mut self) -> Option<Vec<Container>> {
        self.collect_with("docker")
    }

    fn collect_with(&mut self, executable: &str) -> Option<Vec<Container>> {
        self.timed_out = false;
        let ps = match output(executable, &["ps", "--format", "{{json .}}"], self.ps_timeout) {
            Ok(ps) => ps,
            Err(error) => {
                self.timed_out = error.kind() == io::ErrorKind::TimedOut;
                self.stats.clear();
                return None;
            }
        };
        let mut containers = parse_ps(&ps)?;
        if self.checked.is_none_or(|at| at.elapsed() >= Duration::from_secs(10)) {
            self.checked = Some(Instant::now());
            self.stats = output(executable, &["stats", "--no-stream", "--format", "{{json .}}"], self.stats_timeout)
                .ok()
                .and_then(|s| parse_stats(&s))
                .unwrap_or_default();
        }
        for container in &mut containers {
            if let Some(stats) = self.stats.get(&container.name) {
                container.cpu_percent = stats.cpu_percent;
                container.memory = Some(stats.memory.clone());
            }
        }
        Some(containers)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Ps {
    names: String,
    image: String,
    state: String,
    status: String,
    ports: String,
}

fn bounded(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(512).collect()
}

fn parse_ps(text: &str) -> Option<Vec<Container>> {
    let mut containers = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()).take(MAX_CONTAINERS + 1) {
        let ps: Ps = serde_json::from_str(line).ok()?;
        containers.push(Container {
            name: bounded(&ps.names),
            image: bounded(&ps.image),
            state: bounded(&ps.state),
            status: bounded(&ps.status),
            ports: bounded(&ps.ports),
            ..Default::default()
        });
    }
    Some(containers)
}

struct Stats {
    cpu_percent: Option<f32>,
    memory: String,
}

#[derive(Deserialize)]
struct StatsLine {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "CPUPerc")]
    cpu: String,
    #[serde(rename = "MemUsage")]
    memory: String,
}

fn parse_stats(text: &str) -> Option<HashMap<String, Stats>> {
    let mut stats = HashMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()).take(MAX_CONTAINERS) {
        let entry: StatsLine = serde_json::from_str(line).ok()?;
        let cpu_percent = entry.cpu.trim_end_matches('%').parse::<f32>().ok().filter(|n| n.is_finite() && *n >= 0.0);
        stats.insert(bounded(&entry.name), Stats { cpu_percent, memory: bounded(&entry.memory) });
    }
    Some(stats)
}

pub(super) fn published(text: &str) -> Vec<Port> {
    let mut ports = Vec::new();
    for binding in text.split(',') {
        if ports.len() >= atomics_core::machine::MAX_PORTS {
            break;
        }
        let Some((host, target)) = binding.trim().split_once("->") else { continue };
        if !target.ends_with("/tcp") {
            continue;
        }
        let Some((address, port)) = host.rsplit_once(':') else { continue };
        let address = address.trim_matches(['[', ']']);
        if address.parse::<std::net::IpAddr>().is_err() {
            continue;
        }
        // A bounded range also covers docker ps's compact multi-port bindings.
        let (start, end) = port.split_once('-').unwrap_or((port, port));
        let (Ok(start), Ok(end)) = (start.parse::<u16>(), end.parse::<u16>()) else { continue };
        for port in start.max(1)..=end.min(start.saturating_add(511)) {
            if ports.len() >= atomics_core::machine::MAX_PORTS {
                break;
            }
            let mut entry = Port { address: address.into(), port, ..Default::default() };
            entry.previewable = entry.loopback_target();
            ports.push(entry);
        }
    }
    ports
}

/// Bounded output and wall time, including a command that leaves a child holding its stdout open.
/// No shell is used; failures are represented by unavailable data, without logging every sample.
pub(super) fn output(program: &str, args: &[&str], timeout: Duration) -> io::Result<String> {
    let mut child =
        Command::new(program).args(args).stdout(Stdio::piped()).stderr(Stdio::null()).process_group(0).spawn()?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let result = (|| {
        // SAFETY: stdout owns this descriptor; fcntl changes only its nonblocking flag.
        let flags = unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let started = Instant::now();
        let mut bytes = Vec::new();
        loop {
            let mut buffer = [0; 8192];
            loop {
                match stdout.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        bytes.extend_from_slice(&buffer[..n]);
                        if bytes.len() > 512 * 1024 {
                            return Err(io::Error::other("collector output exceeded its limit"));
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    return Err(io::Error::other("collector command failed"));
                }
                // Drain data written between the read and process exit, without blocking on descendants.
                let _ = stdout.take(512 * 1024 - bytes.len() as u64).read_to_end(&mut bytes);
                return String::from_utf8(bytes).map_err(io::Error::other);
            }
            if started.elapsed() >= timeout {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "collector command timed out"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    if result.is_err() {
        // SAFETY: the child was placed in its own process group by process_group(0).
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
        let _ = child.kill();
    }
    let _ = child.wait();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_json_lines_and_published_tcp_bindings() {
        let ps = r#"{"Names":"web-1","Image":"app.example/web:1","State":"running","Status":"Up 2 hours","Ports":"0.0.0.0:8080->80/tcp, [::]:8080->80/tcp, 127.0.0.1:5432->5432/tcp"}
{"Names":"db-1","Image":"postgres:16","State":"running","Status":"Up 1 hour","Ports":"5432/tcp"}"#;
        let containers = parse_ps(ps).unwrap();
        assert_eq!(containers.len(), 2);
        assert_eq!(containers[1].image, "postgres:16");
        let stats = parse_stats(
            r#"{"Name":"web-1","CPUPerc":"2.50%","MemUsage":"32MiB / 2GiB"}
{"Name":"db-1","CPUPerc":"0.01%","MemUsage":"64MiB / 2GiB"}"#,
        )
        .unwrap();
        assert_eq!(stats["web-1"].cpu_percent, Some(2.5));
        assert_eq!(stats["db-1"].memory, "64MiB / 2GiB");
        let ports = published(&containers[0].ports);
        assert_eq!(
            ports.iter().map(|p| (p.port, p.previewable)).collect::<Vec<_>>(),
            [(8080, true), (8080, true), (5432, true)]
        );
        assert!(published("53/udp, 0.0.0.0:53->53/udp, invalid").is_empty());
        assert_eq!(published("127.0.0.1:8000-8002->80-82/tcp").len(), 3);
        assert!(parse_ps("bad json").is_none());
        assert!(parse_stats("bad json").is_none());
    }

    fn executable(script: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docker");
        crate::test_support::write_program(&path, format!("#!/bin/sh\n{script}\n"));
        let path = path.to_string_lossy().into_owned();
        (dir, path)
    }

    #[test]
    fn missing_failing_and_timed_out_docker_have_distinct_status() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Docker::default().collect_with(dir.path().join("missing").to_str().unwrap()).is_none());
        let (_dir, path) = executable("exit 1");
        let mut failed = Docker::default();
        assert!(failed.collect_with(&path).is_none());
        assert!(!failed.timed_out);
        let (_dir, path) = executable("sleep 10");
        let started = Instant::now();
        let mut hung = Docker { ps_timeout: Duration::from_millis(300), ..Docker::default() };
        assert!(hung.collect_with(&path).is_none());
        assert!(hung.timed_out);
        assert!(started.elapsed() < Duration::from_secs(3));
        let (_dir, path) = executable("exit 0");
        assert_eq!(hung.collect_with(&path), Some(Vec::new()));
        assert!(!hung.timed_out);
    }

    #[test]
    fn stats_are_cached_for_ten_seconds_and_ps_refreshes_each_sample() {
        let (_dir, path) = executable(
            r#"if [ "$1" = stats ]; then
printf '%s\n' '{"Name":"web-1","CPUPerc":"3.0%","MemUsage":"32MiB / 2GiB"}'
else
printf '%s\n' '{"Names":"web-1","Image":"app.example/web:1","State":"running","Status":"Up","Ports":""}'
fi"#,
        );
        let mut docker = Docker::default();
        assert_eq!(docker.collect_with(&path).unwrap()[0].cpu_percent, Some(3.0));
        let at = docker.checked;
        let (_dir, path) = executable(
            r#"[ "$1" = stats ] && exit 1
printf '%s\n' '{"Names":"web-1","Image":"app.example/web:2","State":"running","Status":"Up","Ports":""}'"#,
        );
        let fresh = docker.collect_with(&path).unwrap();
        assert_eq!(fresh[0].image, "app.example/web:2");
        assert_eq!(fresh[0].cpu_percent, Some(3.0));
        assert_eq!(docker.checked, at);
        docker.checked = Some(Instant::now() - Duration::from_secs(10));
        assert_eq!(docker.collect_with(&path).unwrap()[0].cpu_percent, None);
    }
}
