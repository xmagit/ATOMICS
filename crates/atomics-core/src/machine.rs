//! An opt-in view of the host machine, outside the board's cursor and replay history.
use serde::{Deserialize, Serialize};

pub const MAX_PORTS: usize = 512;
pub const MAX_CONTAINERS: usize = 256;
pub const MAX_DISKS: usize = 128;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Machine {
    #[serde(default)]
    pub host_name: String,
    pub sampled_at: i64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    pub cores: usize,
    pub load: Option<[f64; 3]>,
    pub cpu_busy: Option<f32>,
    pub memory: Option<Memory>,
    pub disks: Vec<Disk>,
    /// A timeout is distinct from Docker being absent or failing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub containers_timed_out: bool,
    /// `None` means Docker was missing, failed, or timed out (see `containers_timed_out`).
    pub containers: Option<Vec<Container>>,
    pub ports: Vec<Port>,
}

impl Machine {
    /// The relay connects to 127.0.0.1, falling back to ::1. Interface-specific listeners
    /// remain visible without granting a target.
    pub fn previewable(&self, port: u16) -> bool {
        port != 0 && self.ports.iter().any(|p| p.port == port && p.previewable && p.loopback_target())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    pub total: u64,
    pub available: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disk {
    pub path: String,
    pub total: u64,
    pub free: u64,
}

pub const CLEAN_BUILD_CACHES: &str = "Clean build caches of idle worktrees";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildOutput {
    pub path: String,
    pub bytes: u64,
    /// Why cleanup refuses this output; absent for eligible folders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskWarning {
    pub path: String,
    pub free: u64,
    pub blocked: bool,
    pub largest: Vec<BuildOutput>,
}

impl DiskWarning {
    pub fn of(disk: &Disk, resources: &crate::governor::ResourceConfig, largest: Vec<BuildOutput>) -> Option<Self> {
        let gib = 1024u64.pow(3);
        let blocked = disk.free < resources.disk_min_gib.saturating_mul(gib);
        (blocked || disk.free < resources.disk_warn_gib.saturating_mul(gib)).then(|| Self {
            path: disk.path.clone(),
            free: disk.free,
            blocked,
            largest,
        })
    }

    pub fn message(&self) -> String {
        let largest = if self.largest.is_empty() {
            "no build-output folders found yet".into()
        } else {
            self.largest
                .iter()
                .map(|folder| {
                    let size = if folder.bytes == 0 && folder.reason.is_some() {
                        String::new()
                    } else {
                        format!(" ({:.2} GiB)", folder.bytes as f64 / 1024f64.powi(3))
                    };
                    let reason = folder.reason.as_ref().map(|reason| format!("; kept: {reason}")).unwrap_or_default();
                    format!("{}{size}{reason}", folder.path)
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        format!(
            "Low disk space at {}: {:.1} GiB free. {}Largest build-output folders: {largest}. {CLEAN_BUILD_CACHES}.",
            self.path,
            self.free as f64 / 1024f64.powi(3),
            if self.blocked {
                "New tasks and agent sessions are paused; shells, the chief and resumes remain available. "
            } else {
                ""
            }
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Container {
    pub name: String,
    pub image: String,
    pub state: String,
    pub status: String,
    pub ports: String,
    pub cpu_percent: Option<f32>,
    pub memory: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Port {
    pub address: String,
    pub port: u16,
    pub pid: Option<u32>,
    pub process: Option<String>,
    pub session: Option<String>,
    pub previewable: bool,
}

impl Port {
    pub fn loopback_target(&self) -> bool {
        self.address.parse::<std::net::IpAddr>().is_ok_and(|ip| {
            ip.is_unspecified() || ip == std::net::Ipv4Addr::LOCALHOST || ip == std::net::Ipv6Addr::LOCALHOST
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_guard_warns_then_blocks_with_strict_thresholds() {
        let config = crate::governor::ResourceConfig::default();
        let disk = |gib| Disk { path: "/worktrees".into(), free: gib * 1024u64.pow(3), total: 100 * 1024u64.pow(3) };
        assert!(DiskWarning::of(&disk(30), &config, vec![]).is_none());
        assert!(!DiskWarning::of(&disk(10), &config, vec![]).unwrap().blocked);
        let warning = DiskWarning::of(
            &disk(9),
            &config,
            vec![BuildOutput { path: "/worktrees/task/target".into(), bytes: 1024u64.pow(3), reason: None }],
        )
        .unwrap();
        assert!(warning.blocked);
        assert!(warning.message().contains("task/target (1.00 GiB)"));
        assert!(warning.message().contains(CLEAN_BUILD_CACHES));
    }

    #[test]
    fn only_snapshot_listeners_reachable_through_the_loopback_relay_are_previewable() {
        for (address, allowed) in [
            ("127.0.0.1", true),
            ("0.0.0.0", true),
            ("192.0.2.1", false),
            ("::1", true),
            ("::", true),
            ("127.0.0.2", false),
        ] {
            let machine = Machine {
                ports: vec![Port { address: address.into(), port: 8080, previewable: true, ..Default::default() }],
                ..Default::default()
            };
            assert_eq!(machine.previewable(8080), allowed);
            assert!(!machine.previewable(8081));
            assert!(!machine.previewable(0));
        }
    }
}
