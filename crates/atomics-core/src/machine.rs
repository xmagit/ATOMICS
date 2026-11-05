//! An opt-in view of the host machine, outside the board's cursor and replay history.
use serde::{Deserialize, Serialize};

pub const MAX_PORTS: usize = 512;
pub const MAX_CONTAINERS: usize = 256;
pub const MAX_DISKS: usize = 128;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Machine {
    pub sampled_at: i64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    pub cores: usize,
    pub load: Option<[f64; 3]>,
    pub cpu_busy: Option<f32>,
    pub memory: Option<Memory>,
    pub disks: Vec<Disk>,
    /// `None` means Docker was missing, failed, or did not answer in time.
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
