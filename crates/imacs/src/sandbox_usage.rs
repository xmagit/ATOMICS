//! Host VM overhead, sampled off the UI thread. Never use the `sbx run` client's tiny RSS.
use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub bytes: u64,
    /// ps %CPU: 100% is one host core, and a VM can exceed it.
    pub cpu: f64,
}

impl Usage {
    pub fn label(self) -> String {
        format!("{:.1} GiB · {:.0}% CPU", self.bytes as f64 / 1_073_741_824.0, self.cpu)
    }
}

#[derive(Default)]
pub struct Sampler {
    pub usage: HashMap<String, Usage>,
    pub error: Option<String>,
    sampled: Option<Instant>,
    pending: Option<Receiver<Result<HashMap<String, Usage>, String>>>,
}

impl Sampler {
    pub fn total(&self) -> Usage {
        self.usage.values().fold(Usage::default(), |a, b| Usage { bytes: a.bytes + b.bytes, cpu: a.cpu + b.cpu })
    }

    pub fn poll(&mut self, names: Vec<String>, ctx: &egui::Context) -> bool {
        let mut changed = false;
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(result) => {
                    self.pending = None;
                    self.sampled = Some(Instant::now());
                    match result {
                        Ok(usage) => {
                            self.usage = usage;
                            self.error = None;
                        }
                        Err(error) => {
                            self.usage.clear();
                            self.error = Some(error);
                        }
                    }
                    changed = true;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending = None;
                    self.sampled = Some(Instant::now());
                    self.usage.clear();
                    self.error = Some("Host process sampler stopped".into());
                    changed = true;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        self.usage.retain(|name, _| names.contains(name));
        if names.is_empty() {
            self.usage.clear();
            self.error = None;
            return changed;
        }
        ctx.request_repaint_after(Duration::from_secs(2));
        if self.pending.is_none() && self.sampled.is_none_or(|at| at.elapsed() >= Duration::from_secs(2)) {
            let (tx, rx) = mpsc::channel();
            self.pending = Some(rx);
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let result = Command::new("ps")
                    .args(["-axo", "pid=,ppid=,rss=,%cpu=,command="])
                    .env("LC_ALL", "C")
                    .output()
                    .map_err(|error| error.to_string())
                    .and_then(|output| {
                        if !output.status.success() {
                            return Err("Cannot read host VM processes".into());
                        }
                        Ok(parse(&String::from_utf8_lossy(&output.stdout), &names))
                    });
                let _ = tx.send(result);
                ctx.request_repaint();
            });
        }
        changed
    }
}

struct Process<'a> {
    pid: u32,
    parent: u32,
    usage: Usage,
    command: &'a str,
}

fn process(line: &str) -> Option<Process<'_>> {
    let mut rest = line.trim();
    let mut field = || {
        let end = rest.find(char::is_whitespace)?;
        let value = &rest[..end];
        rest = rest[end..].trim_start();
        Some(value)
    };
    let pid = field()?.parse().ok()?;
    let parent = field()?.parse().ok()?;
    let bytes = field()?.parse::<u64>().ok()?.checked_mul(1024)?;
    let cpu = field()?.parse::<f64>().ok()?;
    if !cpu.is_finite() || cpu < 0.0 || rest.is_empty() {
        return None;
    }
    Some(Process { pid, parent, usage: Usage { bytes, cpu }, command: rest })
}

/// PoC process shape: a named VM root (nerdbox/sailor/vfkit/qemu), plus its descendants.
/// Match complete name components so `imacs-a` never claims `imacs-ab` or the sbx CLI.
pub fn parse(ps: &str, names: &[String]) -> HashMap<String, Usage> {
    let processes: Vec<_> = ps.lines().filter_map(process).collect();
    let mut owners = HashMap::new();
    for p in &processes {
        let executable = p.command.split_whitespace().next().unwrap_or("");
        if !["nerdbox", "sailor", "vfkit", "qemu", "cloud-hypervisor", "krun", "sbx-vm"]
            .iter()
            .any(|vm| executable.contains(vm))
        {
            continue;
        }
        for name in names {
            if p.command.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_')).any(|part| part == name)
            {
                owners.insert(p.pid, name.clone());
                break;
            }
        }
    }
    // Resolve ancestors, including out-of-order rows. Each PID contributes exactly once.
    let parents: HashMap<_, _> = processes.iter().map(|p| (p.pid, p.parent)).collect();
    let mut usage: HashMap<String, Usage> = HashMap::new();
    for p in &processes {
        let mut pid = p.pid;
        let mut seen = HashSet::new();
        while seen.insert(pid) {
            if let Some(name) = owners.get(&pid) {
                let total = usage.entry(name.clone()).or_default();
                total.bytes = total.bytes.saturating_add(p.usage.bytes);
                total.cpu += p.usage.cpu;
                break;
            }
            let Some(parent) = parents.get(&pid).filter(|parent| **parent != 0) else { break };
            pid = *parent;
        }
    }
    usage
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn samples_drop_closed_sessions_and_errors_clear_stale_metrics() {
        let ctx = egui::Context::default();
        let (tx, rx) = mpsc::channel();
        let mut sampler = Sampler { pending: Some(rx), ..Default::default() };
        tx.send(Ok(HashMap::from([
            ("imacs-live".into(), Usage { bytes: 1024, cpu: 3.0 }),
            ("imacs-closed".into(), Usage { bytes: 4096, cpu: 8.0 }),
        ])))
        .unwrap();
        assert!(sampler.poll(vec!["imacs-live".into()], &ctx));
        assert_eq!(sampler.total(), Usage { bytes: 1024, cpu: 3.0 });
        let (tx, rx) = mpsc::channel();
        sampler.pending = Some(rx);
        tx.send(Err("ps failed".into())).unwrap();
        assert!(sampler.poll(vec!["imacs-live".into()], &ctx));
        assert!(sampler.usage.is_empty());
        assert_eq!(sampler.error.as_deref(), Some("ps failed"));
        sampler.poll(vec![], &ctx);
        assert!(sampler.error.is_none());
        assert!(sampler.pending.is_none(), "no process sampling without sandbox sessions");
    }
    #[test]
    fn recorded_host_vm_trees_exclude_clients_and_do_not_double_count() {
        let ps = include_str!("../tests/fixtures/sandbox-ps.txt");
        let usage = parse(ps, &["imacs-a".into(), "imacs-ab".into(), "imacs-stopped".into()]);
        assert_eq!(usage.len(), 2);
        assert_eq!(usage["imacs-a"], Usage { bytes: (1_048_576 + 2048 + 1024) * 1024, cpu: 153.0 });
        assert_eq!(usage["imacs-ab"], Usage { bytes: 524_288 * 1024, cpu: 3.0 });
        let sampler = Sampler { usage, ..Default::default() };
        assert_eq!(sampler.total().cpu, 156.0);
        assert_eq!(sampler.total().bytes, (1_048_576 + 2048 + 1024 + 524_288) * 1024);
    }
    #[test]
    fn missing_or_malformed_metrics_are_unavailable() {
        assert!(parse("", &["imacs-a".into()]).is_empty());
        for row in [
            "PID PPID RSS %CPU COMMAND",
            "1 0 -1 0 vfkit imacs-a",
            "1 0 100 NaN vfkit imacs-a",
            "1 0 1 -2 vfkit imacs-a",
            "truncated",
        ] {
            assert!(parse(row, &["imacs-a".into()]).is_empty(), "{row}");
        }
        assert_eq!(Usage { bytes: 1_073_741_824, cpu: 123.4 }.label(), "1.0 GiB · 123% CPU");
    }
}
