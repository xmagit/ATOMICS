use atomics_core::machine::{Disk, Memory, Port};
use std::path::PathBuf;

pub(super) fn load() -> Option<[f64; 3]> {
    let mut values = [0.0; 3];
    // SAFETY: values has room for the requested three load averages.
    (unsafe { libc::getloadavg(values.as_mut_ptr(), 3) } == 3).then_some(values)
}

#[allow(clippy::unnecessary_cast)] // statvfs field widths vary across Unix targets.
pub(super) fn disk(path: PathBuf) -> Option<Disk> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: the path is NUL-terminated and stat points to a full writable statvfs structure.
    if unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statvfs initialized stat on success.
    let stat = unsafe { stat.assume_init() };
    Some(Disk {
        path: path.to_string_lossy().chars().take(1024).collect(),
        total: (stat.f_blocks as u64).saturating_mul(stat.f_frsize as u64),
        free: (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64),
    })
}

#[cfg(any(target_os = "linux", test))]
fn proc_cpu(text: &str) -> Option<(u64, u64)> {
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    // guest and guest_nice are already included in user and nice.
    let fields: Vec<u64> = line.split_whitespace().skip(1).take(8).map(str::parse).collect::<Result<_, _>>().ok()?;
    if fields.len() < 4 {
        return None;
    }
    Some((fields.iter().sum(), fields[3] + fields.get(4).copied().unwrap_or(0)))
}

#[cfg(any(target_os = "linux", test))]
fn proc_memory(text: &str) -> Option<Memory> {
    let field = |name: &str| {
        text.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key == name).then(|| value.split_whitespace().next()?.parse::<u64>().ok()?.checked_mul(1024))?
        })
    };
    let total = field("MemTotal")?;
    Some(Memory { total, available: field("MemAvailable")?.min(total) })
}

#[cfg(target_os = "linux")]
pub(super) fn cpu() -> Option<(u64, u64)> {
    proc_cpu(&std::fs::read_to_string("/proc/stat").ok()?)
}

#[cfg(target_os = "linux")]
pub(super) fn memory() -> Option<Memory> {
    proc_memory(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

#[cfg(target_os = "linux")]
pub(super) fn ports(roots: &[(String, Option<u32>)]) -> Vec<Port> {
    crate::preview::detect::machine_at(roots, std::path::Path::new("/proc"))
}

#[cfg(target_os = "macos")]
pub(super) fn cpu() -> Option<(u64, u64)> {
    let mut info = std::mem::MaybeUninit::<libc::host_cpu_load_info>::uninit();
    let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
    // SAFETY: info has the size requested by HOST_CPU_LOAD_INFO_COUNT, and count is writable.
    let result = unsafe {
        let host = mach2::mach_init::mach_host_self();
        let result = libc::host_statistics(host, libc::HOST_CPU_LOAD_INFO, info.as_mut_ptr().cast(), &mut count);
        mach2::mach_port::mach_port_deallocate(mach2::traps::mach_task_self(), host);
        result
    };
    if result != libc::KERN_SUCCESS {
        return None;
    }
    // SAFETY: host_statistics initialized the CPU load structure.
    let ticks = unsafe { info.assume_init() }.cpu_ticks;
    Some((ticks.iter().map(|n| *n as u64).sum(), ticks[libc::CPU_STATE_IDLE as usize] as u64))
}

#[cfg(target_os = "macos")]
pub(super) fn memory() -> Option<Memory> {
    let mut total = 0u64;
    let mut size = std::mem::size_of_val(&total);
    let mut info = std::mem::MaybeUninit::<libc::vm_statistics64>::uninit();
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: both calls receive writable buffers of the documented size; hw.memsize is a u64.
    let result = unsafe {
        if libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&mut total as *mut u64).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return None;
        }
        let host = mach2::mach_init::mach_host_self();
        let result = libc::host_statistics64(host, libc::HOST_VM_INFO64, info.as_mut_ptr().cast(), &mut count);
        mach2::mach_port::mach_port_deallocate(mach2::traps::mach_task_self(), host);
        result
    };
    if result != libc::KERN_SUCCESS {
        return None;
    }
    // SAFETY: host_statistics64 initialized info; sysconf has no pointer arguments.
    let info = unsafe { info.assume_init() };
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return None;
    }
    // Reclaimable inactive pages plus free pages; speculative pages are already counted as free.
    let available = (info.free_count as u64 + info.inactive_count as u64).saturating_mul(page_size as u64);
    Some(Memory { total, available: available.min(total) })
}

#[cfg(target_os = "macos")]
pub(super) fn ports(roots: &[(String, Option<u32>)]) -> Vec<Port> {
    // lsof is shipped with macOS. Permission-denied owners simply do not appear.
    let mut ports = super::docker::output(
        "lsof",
        &["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpcn"],
        std::time::Duration::from_millis(800),
    )
    .map(|text| lsof_ports(&text))
    .unwrap_or_default();
    if !roots.is_empty()
        && let Ok(parents) = super::docker::output("ps", &["-axo", "pid=,ppid="], std::time::Duration::from_millis(500))
    {
        assign_sessions(&mut ports, roots, &parents);
    }
    ports
}

#[cfg(any(target_os = "macos", test))]
fn assign_sessions(ports: &mut [Port], roots: &[(String, Option<u32>)], table: &str) {
    let parents: std::collections::HashMap<u32, u32> = table
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
        })
        .collect();
    for port in ports {
        let mut pid = port.pid;
        let mut visited = std::collections::HashSet::new();
        while let Some(current) = pid {
            if !visited.insert(current) {
                break;
            }
            if let Some((id, _)) = roots.iter().find(|(_, root)| *root == Some(current)) {
                port.session = Some(id.clone());
                break;
            }
            pid = parents.get(&current).copied();
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn lsof_ports(text: &str) -> Vec<Port> {
    let (mut pid, mut process) = (None, None);
    let mut ports = Vec::new();
    for line in text.lines() {
        let Some((field, value)) = line.split_at_checked(1) else { continue };
        match field {
            "p" => {
                pid = value.parse().ok();
                process = None;
            }
            "c" => process = Some(value.chars().take(128).collect::<String>()),
            "n" => {
                let Some((address, port)) = value.rsplit_once(':') else { continue };
                let Ok(port) = port.parse::<u16>() else { continue };
                let address = match address {
                    "*" => "0.0.0.0",
                    address => address.trim_matches(['[', ']']),
                };
                let mut entry =
                    Port { address: address.into(), port, pid, process: process.clone(), ..Default::default() };
                entry.previewable = entry.loopback_target();
                ports.push(entry);
            }
            _ => {}
        }
    }
    ports
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn cpu() -> Option<(u64, u64)> {
    None
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn memory() -> Option<Memory> {
    None
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn ports(_roots: &[(String, Option<u32>)]) -> Vec<Port> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_cpu_and_memory_parsers_use_host_totals_and_available_memory() {
        assert_eq!(proc_cpu("cpu  100 20 30 400 50 6 7 8 9 10\ncpu0 1 2 3 4"), Some((621, 450)));
        assert_eq!(proc_cpu("cpu0 1 2 3 4"), None);
        assert_eq!(proc_cpu("cpu 1 2"), None);
        assert_eq!(
            proc_memory("MemTotal: 8192 kB\nMemFree: 100 kB\nMemAvailable: 4096 kB\n"),
            Some(Memory { total: 8192 * 1024, available: 4096 * 1024 })
        );
        assert_eq!(proc_memory("MemTotal: 8192 kB\n"), None);
    }

    #[test]
    fn macos_listener_fields_preserve_owner_and_bind_address() {
        let mut ports =
            lsof_ports("p123\ncweb\nn127.0.0.1:8080\nn*:8081\np124\ncworker\nn[::1]:8082\nn192.0.2.1:8083\n");
        assign_sessions(&mut ports, &[("agent-1".into(), Some(100))], "123 101\n101 100\n100 1\n124 1\n1 1\n");
        assert_eq!(ports[0].session.as_deref(), Some("agent-1"));
        assert_eq!(ports[2].session, None);
        assert_eq!(ports.len(), 4);
        assert_eq!(ports[0].pid, Some(123));
        assert_eq!(ports[0].process.as_deref(), Some("web"));
        assert!(ports[0].previewable && ports[1].previewable);
        assert!(ports[2].previewable && !ports[3].previewable);
    }

    #[test]
    fn native_platform_samples_resources_and_the_home_filesystem() {
        assert!(load().is_some());
        assert!(cpu().is_some());
        let memory = memory().unwrap();
        assert!(memory.total > 0 && memory.available <= memory.total);
        let disk = disk(std::env::temp_dir()).unwrap();
        assert!(disk.total > 0 && disk.free <= disk.total);
    }
}
