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
    pub fn offers(&self, listener: &Listener, port: u16) -> bool {
        // Two eligible sockets of the same family could receive the same loopback dial.
        let Some(ip) = listener.loopback_ip() else { return false };
        let mut targets = self.ports.iter().filter(|p| p.port == port && p.loopback_ip() == Some(ip));
        port != 0
            && targets.next().is_some_and(|p| p.previewable && p.identifiable() && listener.matches(p))
            && targets.next().is_none()
    }

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
    /// Where the filesystem is mounted, which names it in warnings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount: Option<String>,
    /// Memory-backed (tmpfs, ramfs): its space is the machine's memory.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub memory: bool,
}

/// What a measured place holds, which decides how much free space it needs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Holds {
    /// Only folders sessions run in: it warns when its share of free space runs low.
    #[default]
    Other,
    /// Checkouts or build output: it must also keep `disk_warn_gib` free for the work.
    Work,
    /// Where new worktrees go or a session starts: judged as work even when memory-backed.
    Worktrees,
}

/// Any place warns below this share of free space, whatever it holds.
pub const WARN_FREE_PERCENT: u64 = 10;

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
    /// A place warns below [`WARN_FREE_PERCENT`] free, and one that holds work also below `disk_warn_gib`; only
    /// work is refused, below `disk_min_gib`. A memory-backed place counts only when new worktrees go there.
    pub fn of(
        disk: &Disk,
        holds: Holds,
        resources: &crate::governor::ResourceConfig,
        largest: Vec<BuildOutput>,
    ) -> Option<Self> {
        if disk.memory && holds != Holds::Worktrees {
            return None;
        }
        let gib = 1024u64.pow(3);
        let work = holds != Holds::Other;
        let blocked = work && disk.free < resources.disk_min_gib.saturating_mul(gib);
        let scarce = u128::from(disk.free) * 100 < u128::from(disk.total) * u128::from(WARN_FREE_PERCENT);
        (blocked || scarce || work && disk.free < resources.disk_warn_gib.saturating_mul(gib)).then(|| Self {
            path: disk.mount.clone().unwrap_or_else(|| disk.path.clone()),
            free: disk.free,
            blocked,
            largest,
        })
    }

    /// Whether this warning is news after `before` of the same place: it now refuses work, or at least a tenth
    /// of what was free then (and at least 1 GiB) is gone.
    pub fn worse_than(&self, before: &Self) -> bool {
        let margin = (before.free / 10).max(1024u64.pow(3));
        self.blocked && !before.blocked || self.free.saturating_add(margin) <= before.free
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
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
    /// Socket inode or published container ID when the collector can identify the listener directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listener_id: Option<String>,
    pub address: String,
    pub port: u16,
    pub pid: Option<u32>,
    pub process: Option<String>,
    pub session: Option<String>,
    pub previewable: bool,
}

/// Source identity carried by a Machine tunnel. The process display name is only for the board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listener {
    pub address: String,
    pub pid: Option<u32>,
    pub listener_id: Option<String>,
    pub session: Option<String>,
}

impl From<&Port> for Listener {
    fn from(p: &Port) -> Self {
        Self { address: p.address.clone(), pid: p.pid, listener_id: p.listener_id.clone(), session: p.session.clone() }
    }
}

/// The address a browser or a tunnel can dial for this bind address.
fn loopback_ip(address: &str) -> Option<std::net::IpAddr> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    match address.parse().ok()? {
        IpAddr::V4(ip) if ip.is_unspecified() || ip == Ipv4Addr::LOCALHOST => Some(Ipv4Addr::LOCALHOST.into()),
        IpAddr::V6(ip) if ip.is_unspecified() || ip == Ipv6Addr::LOCALHOST => Some(Ipv6Addr::LOCALHOST.into()),
        _ => None,
    }
}

impl Listener {
    pub fn loopback_ip(&self) -> Option<std::net::IpAddr> {
        loopback_ip(&self.address)
    }

    fn matches(&self, p: &Port) -> bool {
        self.address == p.address && self.pid == p.pid && self.listener_id == p.listener_id && self.session == p.session
    }
}

impl Port {
    pub fn identifiable(&self) -> bool {
        self.pid.is_some() || self.listener_id.is_some()
    }

    pub fn loopback_ip(&self) -> Option<std::net::IpAddr> {
        loopback_ip(&self.address)
    }

    pub fn loopback_target(&self) -> bool {
        self.loopback_ip().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn disk(free_gib: u64, total_gib: u64) -> Disk {
        Disk { path: "/worktrees".into(), free: free_gib * GIB, total: total_gib * GIB, ..Disk::default() }
    }

    #[test]
    fn disk_guard_warns_then_blocks_with_strict_thresholds() {
        let config = crate::governor::ResourceConfig::default();
        let disk = |gib| disk(gib, 100);
        assert!(DiskWarning::of(&disk(30), Holds::Worktrees, &config, vec![]).is_none());
        assert!(!DiskWarning::of(&disk(10), Holds::Worktrees, &config, vec![]).unwrap().blocked);
        let warning = DiskWarning::of(
            &disk(9),
            Holds::Worktrees,
            &config,
            vec![BuildOutput { path: "/worktrees/task/target".into(), bytes: 1024u64.pow(3), reason: None }],
        )
        .unwrap();
        assert!(warning.blocked);
        assert!(warning.message().contains("task/target (1.00 GiB)"));
        assert!(warning.message().contains(CLEAN_BUILD_CACHES));
    }

    #[test]
    fn a_place_without_work_warns_only_when_its_share_of_free_space_runs_low() {
        let config = crate::governor::ResourceConfig::default();
        // 14.6 GiB of a 16 GiB folder's disk is plenty when no worktree or build output lives there.
        let roomy = Disk { free: 14 * GIB + GIB / 2, ..disk(0, 16) };
        assert!(DiskWarning::of(&roomy, Holds::Other, &config, vec![]).is_none());
        assert!(DiskWarning::of(&roomy, Holds::Work, &config, vec![]).is_some_and(|w| !w.blocked));
        // Under a tenth free, it warns, but it refuses nothing: no work goes there.
        let full = DiskWarning::of(&disk(1, 16), Holds::Other, &config, vec![]).unwrap();
        assert!(!full.blocked);
        assert!(DiskWarning::of(&disk(1, 16), Holds::Work, &config, vec![]).unwrap().blocked);
        // A large disk holding work warns at a tenth free even above `disk_warn_gib`.
        assert!(DiskWarning::of(&disk(150, 2000), Holds::Work, &config, vec![]).is_some_and(|w| !w.blocked));
        assert!(DiskWarning::of(&disk(250, 2000), Holds::Work, &config, vec![]).is_none());
    }

    #[test]
    fn memory_backed_places_count_only_where_new_worktrees_go_and_warnings_name_the_mount() {
        let config = crate::governor::ResourceConfig::default();
        let tmpfs = Disk {
            path: "/tmp/scratch/0f3a9c2e-5b7d-4e1f-9a2b-3c4d5e6f7a8b/worktree".into(),
            mount: Some("/tmp".into()),
            memory: true,
            ..disk(1, 16)
        };
        assert!(DiskWarning::of(&tmpfs, Holds::Other, &config, vec![]).is_none());
        assert!(DiskWarning::of(&tmpfs, Holds::Work, &config, vec![]).is_none());
        let warning = DiskWarning::of(&tmpfs, Holds::Worktrees, &config, vec![]).unwrap();
        assert!(warning.blocked);
        assert_eq!(warning.path, "/tmp");
        assert!(warning.message().starts_with("Low disk space at /tmp: 1.0 GiB free."));
    }

    #[test]
    fn a_warning_is_worse_once_it_refuses_work_or_a_tenth_of_the_free_space_is_gone() {
        let config = crate::governor::ResourceConfig::default();
        let at = |free: u64| DiskWarning::of(&Disk { free, ..disk(0, 100) }, Holds::Work, &config, vec![]).unwrap();
        let before = at(20 * GIB);
        assert!(!at(20 * GIB).worse_than(&before));
        assert!(!at(18 * GIB + 1).worse_than(&before), "small swings during builds are no news");
        assert!(at(18 * GIB).worse_than(&before));
        assert!(!at(25 * GIB).worse_than(&before));
        // Below 10 GiB a tenth is less than the 1 GiB that counts.
        let refused = at(9 * GIB);
        assert!(refused.blocked);
        assert!(!at(8 * GIB + 1).worse_than(&refused));
        assert!(at(8 * GIB).worse_than(&refused));
        // Starting to refuse work is news, however little changed.
        assert!(at(10 * GIB - 1).worse_than(&at(10 * GIB)));
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
