//! Linux /proc listener ownership. Discovery never probes a port.
#[cfg(any(target_os = "linux", test))]
use atomics_core::machine::Port;
use atomics_core::preview::Preview;
use std::collections::HashMap;

#[cfg(any(target_os = "linux", test))]
use std::collections::HashSet;

#[cfg(any(target_os = "linux", test))]
fn listeners(table: &str, ipv6: bool) -> Vec<(u64, Port)> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 10 || fields[3] != "0A" {
                return None;
            }
            let (address, port) = fields[1].split_once(':')?;
            let address = if ipv6 {
                if address.len() != 32 {
                    return None;
                }
                let mut octets = [0; 16];
                for (i, part) in octets.chunks_mut(4).enumerate() {
                    part.copy_from_slice(&u32::from_str_radix(&address[i * 8..i * 8 + 8], 16).ok()?.to_ne_bytes());
                }
                let ip = std::net::Ipv6Addr::from(octets);
                ip.to_string()
            } else {
                let ip = std::net::Ipv4Addr::from(u32::from_str_radix(address, 16).ok()?.to_ne_bytes());
                ip.to_string()
            };
            let port = u16::from_str_radix(port, 16).ok()?;
            let mut entry = Port { address, port, ..Default::default() };
            entry.previewable = entry.loopback_target();
            (port != 0).then(|| Some((fields[9].parse().ok()?, entry)))?
        })
        .collect()
}

#[cfg(any(target_os = "linux", test))]
fn parent(stat: &str) -> Option<u32> {
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(any(target_os = "linux", test))]
fn tree(root: u32, parents: &HashMap<u32, u32>) -> HashSet<u32> {
    let mut pids = HashSet::from([root]);
    loop {
        let before = pids.len();
        for (&pid, ppid) in parents {
            if pids.contains(ppid) {
                pids.insert(pid);
            }
        }
        if pids.len() == before {
            return pids;
        }
    }
}

#[cfg(target_os = "linux")]
pub fn discover(roots: &[(String, Option<u32>)]) -> HashMap<String, Vec<Preview>> {
    discover_at(roots, std::path::Path::new("/proc"))
}

#[cfg(any(target_os = "linux", test))]
fn discover_at(roots: &[(String, Option<u32>)], proc_root: &std::path::Path) -> HashMap<String, Vec<Preview>> {
    if roots.is_empty() {
        return HashMap::new();
    }
    let ports = listener_inodes(proc_root);
    let parents = processes(proc_root).into_iter().map(|(pid, (ppid, _))| (pid, ppid)).collect();
    roots
        .iter()
        .filter_map(|(id, root)| {
            let owned: HashSet<_> =
                tree((*root)?, &parents).into_iter().flat_map(|pid| sockets(proc_root, pid)).collect();
            let mut previews: Vec<_> = ports
                .iter()
                .filter(|(inode, p)| {
                    owned.contains(inode)
                        && p.address.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
                })
                .map(|(_, p)| Preview { port: p.port, path: String::new(), label: String::new() })
                .collect();
            previews.sort_by_key(|p| p.port);
            previews.dedup_by_key(|p| p.port);
            previews.truncate(atomics_core::preview::MAX_PREVIEWS);
            Some((id.clone(), previews))
        })
        .collect()
}

#[cfg(any(target_os = "linux", test))]
fn listener_inodes(proc_root: &std::path::Path) -> Vec<(u64, Port)> {
    let mut ports = Vec::new();
    for (file, ipv6) in [("net/tcp", false), ("net/tcp6", true)] {
        if let Ok(table) = std::fs::read_to_string(proc_root.join(file)) {
            ports.extend(listeners(&table, ipv6));
        }
    }
    ports
}

#[cfg(any(target_os = "linux", test))]
fn processes(proc_root: &std::path::Path) -> HashMap<u32, (u32, Option<String>)> {
    use std::fs;
    fs::read_dir(proc_root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse().ok()?;
            let ppid = parent(&fs::read_to_string(entry.path().join("stat")).ok()?)?;
            let name = fs::read_to_string(entry.path().join("comm")).ok().map(|s| s.trim().chars().take(128).collect());
            Some((pid, (ppid, name)))
        })
        .collect()
}

#[cfg(any(target_os = "linux", test))]
fn sockets(proc_root: &std::path::Path, pid: u32) -> Vec<u64> {
    use std::fs;
    fs::read_dir(proc_root.join(pid.to_string()).join("fd"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let target = fs::read_link(entry.path()).ok()?;
            let text = target.to_str()?;
            text.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
        })
        .collect()
}

/// Scan all TCP listeners once, then join readable descriptors to owners and agent process trees.
/// Unlike ordinary agent discovery, Machine needs descriptor ownership for every readable process.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn machine_at(roots: &[(String, Option<u32>)], proc_root: &std::path::Path) -> Vec<Port> {
    let ports = listener_inodes(proc_root);
    let processes = processes(proc_root);
    let parents = processes.iter().map(|(&pid, (ppid, _))| (pid, *ppid)).collect();
    let trees: Vec<_> = roots.iter().filter_map(|(id, root)| Some((id, tree((*root)?, &parents)))).collect();
    let mut owners = HashMap::new();
    let mut pids: Vec<_> = processes.keys().copied().collect();
    pids.sort();
    for pid in pids {
        for inode in sockets(proc_root, pid) {
            // Prefer an agent owner for a shared socket, then the lowest readable pid.
            let session = trees.iter().find(|(_, pids)| pids.contains(&pid)).map(|(id, _)| (*id).clone());
            let owner = owners.entry(inode).or_insert_with(|| (pid, session.clone()));
            if owner.1.is_none() && session.is_some() {
                *owner = (pid, session);
            }
        }
    }
    ports
        .into_iter()
        .map(|(inode, mut port)| {
            if let Some((pid, session)) = owners.get(&inode) {
                port.pid = Some(*pid);
                port.process = processes.get(pid).and_then(|(_, name)| name.clone());
                port.session = session.clone();
            }
            port
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
pub fn discover(_roots: &[(String, Option<u32>)]) -> HashMap<String, Vec<Preview>> {
    HashMap::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sample_proc_data_limits_listeners_and_follows_descendants() {
        let table = "sl local_address rem_address st tx_queue tr retrnsmt uid timeout inode\n0: 0100007F:1435 00000000:0000 0A 0:0 0:0 0 1000 0 42\n1: 00000000:1F90 00000000:0000 0A 0:0 0:0 0 1000 0 43\n2: 0200000A:0050 00000000:0000 0A 0:0 0:0 0 1000 0 44\n3: 0100007F:1435 00000000:0000 01 0:0 0:0 0 1000 0 45";
        let parsed = listeners(table, false);
        assert_eq!(
            parsed.iter().map(|(inode, p)| (*inode, p.port, p.previewable)).collect::<Vec<_>>(),
            vec![(42, 5173, true), (43, 8080, true), (44, 80, false)]
        );
        let table6 = "header\n0: 00000000000000000000000001000000:1435 0:0 0A 0:0 0:0 0 1000 0 46";
        assert_eq!(listeners(table6, true)[0].1.address, "::1");
        assert!(listeners(table6, true)[0].1.previewable);
        assert_eq!(parent("12 (worker (web)) S 10 1 1"), Some(10));
        assert_eq!(tree(10, &HashMap::from([(12, 10), (14, 12), (20, 1)])), HashSet::from([10, 12, 14]));
        // The same scanner used on Linux maps only descendant descriptors to listener inodes.
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        std::fs::create_dir(root.join("net")).unwrap();
        std::fs::write(root.join("net/tcp"), table).unwrap();
        std::fs::write(root.join("net/tcp6"), table6).unwrap();
        for (pid, ppid, inodes) in [(10, 1, vec![]), (12, 10, vec![42, 44]), (14, 12, vec![46]), (20, 1, vec![43])] {
            let dir = root.join(pid.to_string());
            std::fs::create_dir_all(dir.join("fd")).unwrap();
            std::fs::write(dir.join("comm"), "worker\n").unwrap();
            std::fs::write(dir.join("stat"), format!("{pid} (worker (web)) S {ppid} 1 1")).unwrap();
            for inode in inodes {
                std::os::unix::fs::symlink(format!("socket:[{inode}]"), dir.join("fd").join(inode.to_string()))
                    .unwrap();
            }
        }
        let machine = machine_at(&[("agent".into(), Some(10))], root);
        assert_eq!(machine.len(), 4);
        let owned = machine.iter().find(|p| p.port == 5173 && p.address == "127.0.0.1").unwrap();
        assert_eq!(
            (owned.pid, owned.process.as_deref(), owned.session.as_deref()),
            (Some(12), Some("worker"), Some("agent"))
        );
        assert!(machine.iter().any(|p| p.port == 80 && !p.previewable));
        let found = discover_at(&[("agent".into(), Some(10)), ("child-agent".into(), Some(12))], root);
        assert_eq!(found["child-agent"].iter().map(|p| p.port).collect::<Vec<_>>(), vec![5173]);
        assert_eq!(found["agent"].iter().map(|p| p.port).collect::<Vec<_>>(), vec![5173]);
    }
}
