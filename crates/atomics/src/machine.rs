//! Demand-driven native collection. Only the worker performs filesystem or subprocess I/O.
mod docker;
mod platform;

use atomics_core::machine::{Disk, Machine};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

pub const INTERVAL: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct Collector {
    pub snapshot: Option<Machine>,
    pub desktop_open: AtomicBool,
    pub clients: Arc<AtomicUsize>,
    sampler: Option<Sampler>,
    reading: Option<mpsc::Receiver<(Sampler, Machine)>>,
    checked: Option<Instant>,
}

impl Collector {
    pub fn tick(&mut self, paths: Vec<PathBuf>, roots: Vec<(String, Option<u32>)>, wake: crate::host::Wake) -> bool {
        let mut changed = false;
        if let Some(rx) = &self.reading {
            match rx.try_recv() {
                Ok((sampler, snapshot)) => {
                    self.sampler = Some(sampler);
                    self.snapshot = Some(snapshot);
                    self.reading = None;
                    changed = true;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.reading = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if (self.desktop_open.load(Ordering::Acquire) || self.clients.load(Ordering::Acquire) > 0)
            && self.reading.is_none()
            && self.checked.is_none_or(|at| at.elapsed() >= INTERVAL)
        {
            self.checked = Some(Instant::now());
            let mut sampler = self.sampler.take().unwrap_or_default();
            let (tx, rx) = mpsc::sync_channel(1);
            self.reading = Some(rx);
            let _ = std::thread::Builder::new().name("machine-collector".into()).spawn(move || {
                let snapshot = sampler.collect(paths, &roots);
                let _ = tx.send((sampler, snapshot));
                wake.notify();
            });
        }
        changed
    }
}

#[derive(Default)]
struct Sampler {
    cpu: Option<(u64, u64)>,
    docker: docker::Docker,
}

impl Sampler {
    fn collect(&mut self, mut paths: Vec<PathBuf>, roots: &[(String, Option<u32>)]) -> Machine {
        let cpu = platform::cpu();
        let cpu_busy = cpu.zip(self.cpu).and_then(|(now, before)| cpu_delta(before, now));
        self.cpu = cpu;
        paths.sort();
        paths.dedup();
        let mut truncated = paths.len() > atomics_core::machine::MAX_DISKS;
        let disks: Vec<Disk> =
            paths.into_iter().take(atomics_core::machine::MAX_DISKS).filter_map(platform::disk).collect();
        let mut containers = self.docker.collect();
        if let Some(containers) = &mut containers {
            truncated |= containers.len() > atomics_core::machine::MAX_CONTAINERS;
            containers.truncate(atomics_core::machine::MAX_CONTAINERS);
        }
        let mut ports = platform::ports(roots);
        // Docker may publish with NAT and no userspace listening socket in /proc/net/tcp.
        for container in containers.iter().flatten() {
            for mut port in docker::published(&container.ports) {
                if !ports.iter().any(|p| p.port == port.port && p.address == port.address) {
                    if ports.len() >= atomics_core::machine::MAX_PORTS {
                        truncated = true;
                        break;
                    }
                    port.process = Some(format!("Docker: {}", container.name));
                    ports.push(port);
                }
            }
        }
        ports.sort_by(|a, b| (a.port, &a.address, a.pid).cmp(&(b.port, &b.address, b.pid)));
        ports.dedup_by(|a, b| a.port == b.port && a.address == b.address && a.pid == b.pid);
        truncated |= ports.len() > atomics_core::machine::MAX_PORTS;
        ports.truncate(atomics_core::machine::MAX_PORTS);
        Machine {
            host_name: atomics_core::paths::host_name(),
            containers_timed_out: self.docker.timed_out,
            sampled_at: crate::usage::now(),
            truncated,
            cores: std::thread::available_parallelism().map_or(1, usize::from),
            load: platform::load(),
            cpu_busy,
            memory: platform::memory(),
            disks,
            containers,
            ports,
        }
    }
}

fn cpu_delta(before: (u64, u64), now: (u64, u64)) -> Option<f32> {
    let total = now.0.checked_sub(before.0)?;
    let idle = now.1.checked_sub(before.1)?;
    (total > 0 && idle <= total).then(|| 100.0 * (total - idle) as f32 / total as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_busy_uses_deltas_and_discards_reset_or_empty_samples() {
        assert_eq!(cpu_delta((100, 30), (200, 50)), Some(80.0));
        assert_eq!(cpu_delta((100, 30), (100, 30)), None);
        assert_eq!(cpu_delta((100, 30), (90, 40)), None);
    }

    #[test]
    fn collection_stays_idle_without_an_open_panel() {
        let mut collector = Collector::default();
        assert!(!collector.tick(Vec::new(), Vec::new(), Default::default()));
        assert!(collector.reading.is_none() && collector.checked.is_none());
    }
}
