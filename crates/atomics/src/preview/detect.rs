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

#[cfg(target_os = "linux")]
pub fn discover(roots: &[(String, Option<u32>)]) -> HashMap<String, Vec<Preview>> {
    discover_at(roots, std::path::Path::new("/proc"))
}

/// The agents themselves, as the kernel names their processes.
#[cfg(any(target_os = "linux", test))]
const AGENT_PROGRAMS: &[&str] = &["claude", "codex", "copilot", "cursor-agent"];

#[cfg(any(target_os = "linux", test))]
fn discover_at(roots: &[(String, Option<u32>)], proc_root: &std::path::Path) -> HashMap<String, Vec<Preview>> {
    if roots.is_empty() {
        return HashMap::new();
    }
    let mut found: HashMap<String, Vec<Preview>> = roots.iter().map(|(id, _)| (id.clone(), Vec::new())).collect();
    for port in machine_at(roots, proc_root, None) {
        // Claude Code listens on a port of its own, which shows nothing. Started by its version's file, it has that name.
        let program = || std::fs::read_link(proc_root.join(port.pid?.to_string()).join("exe")).ok();
        let agents_own = port.process.as_deref().is_some_and(|name| AGENT_PROGRAMS.contains(&name))
            || program().is_some_and(|exe| exe.parent().is_some_and(|dir| dir.ends_with("claude/versions")));
        if port.previewable
            && !agents_own
            && let Some(previews) = port.session.as_ref().and_then(|id| found.get_mut(id))
        {
            previews.push(Preview { port: port.port, listener: Some((&port).into()), ..Default::default() });
        }
    }
    for previews in found.values_mut() {
        previews.sort_by_key(|p| p.port);
        previews.dedup_by_key(|p| p.port);
        // Prefer the listener advertised by its own server's command line. Never send probe traffic.
        previews.sort_by_cached_key(|p| {
            let args = p
                .listener
                .as_ref()
                .and_then(|l| l.pid)
                .and_then(|pid| std::fs::read(proc_root.join(pid.to_string()).join("cmdline")).ok());
            !args.is_some_and(|args| advertises_port(&args, p.port))
        });
        previews.truncate(atomics_core::preview::MAX_PREVIEWS);
    }
    found
}

#[cfg(any(target_os = "linux", test))]
fn advertises_port(command: &[u8], port: u16) -> bool {
    let command = String::from_utf8_lossy(command);
    let mut args = command.split('\0').peekable();
    let number = port.to_string();
    while let Some(arg) = args.next() {
        if arg == "--port" && args.peek().copied() == Some(number.as_str()) {
            return true;
        }
        if arg.strip_prefix("--port=") == Some(number.as_str()) {
            return true;
        }
        let value = arg.split_once('=').map_or(arg, |(_, value)| value);
        if value.split(';').any(|entry| {
            (entry.starts_with("http://") || entry.starts_with("https://"))
                && url::Url::parse(entry).is_ok_and(|url| url.port_or_known_default() == Some(port))
        }) {
            return true;
        }
    }
    false
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

/// Join the selected listening sockets to their owners and nearest session roots.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn machine_at(roots: &[(String, Option<u32>)], proc_root: &std::path::Path, port: Option<u16>) -> Vec<Port> {
    let mut ports = listener_inodes(proc_root);
    ports.retain(|(_, p)| port.is_none_or(|n| p.port == n));
    if ports.is_empty() {
        return Vec::new();
    }
    let listening: HashSet<_> = ports.iter().map(|(inode, _)| *inode).collect();
    let processes = processes(proc_root);
    let parents: HashMap<u32, u32> = processes.iter().map(|(&pid, (ppid, _))| (pid, *ppid)).collect();
    let mut owners: HashMap<u64, Vec<(u32, Option<String>)>> = HashMap::new();
    for &pid in processes.keys() {
        let inodes: HashSet<_> = sockets(proc_root, pid).into_iter().filter(|i| listening.contains(i)).collect();
        if inodes.is_empty() {
            continue;
        }
        let mut parent = pid;
        let mut seen = HashSet::new();
        let session = loop {
            if !seen.insert(parent) {
                break None;
            }
            if let Some((id, _)) = roots.iter().find(|(_, root)| *root == Some(parent)) {
                break Some(id.clone());
            }
            let Some(&ppid) = parents.get(&parent) else { break None };
            parent = ppid;
        };
        for inode in inodes {
            owners.entry(inode).or_default().push((pid, session.clone()));
        }
    }
    ports
        .into_iter()
        .map(|(inode, mut port)| {
            port.listener_id = Some(inode.to_string());
            if let Some([(pid, session)]) = owners.get(&inode).map(Vec::as_slice) {
                port.pid = Some(*pid);
                port.process = processes.get(pid).and_then(|(_, name)| name.clone());
                port.session = session.clone();
            } else {
                port.previewable = false;
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
    fn prefers_the_port_the_server_advertises() {
        assert!(advertises_port(b"server\0--port\x005173\0", 5173));
        assert!(advertises_port(b"server\0--port=5173\0", 5173));
        assert!(advertises_port(b"server\0--urls=http://localhost:8123;https://localhost:9443\0", 9443));
        assert!(!advertises_port(b"server\0--port=51730\0", 5173));
    }

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
        let machine = machine_at(&[("agent".into(), Some(10))], root, None);
        assert_eq!(machine.len(), 4);
        let owned = machine.iter().find(|p| p.port == 5173 && p.address == "127.0.0.1").unwrap();
        assert_eq!(
            (owned.pid, owned.process.as_deref(), owned.session.as_deref()),
            (Some(12), Some("worker"), Some("agent"))
        );
        assert!(machine.iter().any(|p| p.port == 80 && !p.previewable));
        let found = discover_at(&[("agent".into(), Some(10)), ("child-agent".into(), Some(12))], root);
        assert_eq!(found["child-agent"].iter().map(|p| p.port).collect::<Vec<_>>(), vec![5173]);
        assert!(found["agent"].is_empty(), "the nested session owns both listeners");
        std::fs::write(root.join("12/comm"), "claude\n").unwrap();
        let found = discover_at(&[("child-agent".into(), Some(12))], root);
        assert!(
            found["child-agent"].iter().all(|p| p.listener.as_ref().unwrap().pid != Some(12)),
            "an agent's own port"
        );
        std::fs::write(root.join("12/comm"), "worker\n").unwrap();
        std::os::unix::fs::symlink("/share/claude/versions/2.1.332", root.join("12/exe")).unwrap();
        let found = discover_at(&[("child-agent".into(), Some(12))], root);
        assert!(found["child-agent"].iter().all(|p| p.listener.as_ref().unwrap().pid != Some(12)), "by its file");
        std::fs::remove_file(root.join("12/exe")).unwrap();
        // Each family belongs to a different process after the descriptor changes hands.
        std::fs::rename(root.join("14/fd/46"), root.join("20/fd/46")).unwrap();
        let found = discover_at(&[("v4".into(), Some(12)), ("v6".into(), Some(20))], root);
        let listener = |id: &str| {
            let preview = found[id].iter().find(|p| p.port == 5173).unwrap();
            serde_json::to_value(preview).unwrap()["listener"].clone()
        };
        assert_eq!(listener("v4")["address"], "127.0.0.1");
        assert_eq!(listener("v6")["address"], "::1");
        assert_eq!(listener("v6")["pid"], 20);
        assert_eq!(listener("v6")["listener_id"], "46");
        std::fs::remove_file(root.join("20/fd/46")).unwrap();
        let ports = machine_at(&[], root, Some(5173));
        assert_eq!(ports.len(), 2);
        assert!(!ports.iter().find(|p| p.listener_id.as_deref() == Some("46")).unwrap().previewable);
    }
}
