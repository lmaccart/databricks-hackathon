#[cfg(any(target_os = "linux", test))]
use std::collections::HashMap;

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuTopology {
    pub package: i32,
    pub core: i32,
}

#[cfg(any(target_os = "linux", test))]
pub fn parse_cpu_list(input: &str) -> Option<Vec<usize>> {
    let mut cpus = Vec::new();
    for item in input.trim().split(',') {
        if item.is_empty() {
            return None;
        }
        if let Some((first, last)) = item.split_once('-') {
            let first = first.parse::<usize>().ok()?;
            let last = last.parse::<usize>().ok()?;
            if first > last {
                return None;
            }
            cpus.extend(first..=last);
        } else {
            cpus.push(item.parse::<usize>().ok()?);
        }
    }
    cpus.sort_unstable();
    cpus.dedup();
    Some(cpus)
}

#[cfg(any(target_os = "linux", test))]
pub fn ordered_allowed_cpus(
    allowed: &[usize],
    online: &[usize],
    topology: &HashMap<usize, CpuTopology>,
) -> Option<Vec<usize>> {
    let online: std::collections::HashSet<_> = online.iter().copied().collect();
    let mut allowed = allowed.to_vec();
    allowed.sort_unstable();
    allowed.dedup();

    let mut cores = std::collections::BTreeMap::<(i32, i32), Vec<usize>>::new();
    for cpu in allowed.into_iter().filter(|cpu| online.contains(cpu)) {
        let core = *topology.get(&cpu)?;
        cores.entry((core.package, core.core)).or_default().push(cpu);
    }
    if cores.is_empty() {
        return None;
    }

    // Visit every physical core before returning to any core for its next SMT sibling.
    let mut ordered = Vec::new();
    for sibling in 0..cores.values().map(Vec::len).max()? {
        for cpus in cores.values() {
            if let Some(&cpu) = cpus.get(sibling) {
                ordered.push(cpu);
            }
        }
    }
    Some(ordered)
}

#[cfg(any(target_os = "linux", test))]
pub fn workers_to_pin(ordered: &[usize], workers: usize, control_allowed: bool) -> Option<Vec<usize>> {
    if !control_allowed || workers == 0 || workers > ordered.len() {
        return None;
    }
    Some(ordered[..workers].to_vec())
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct LinuxTopology {
    pub allowed: Vec<usize>,
    pub ordered: Option<Vec<usize>>,
    pub note: &'static str,
}

#[cfg(target_os = "linux")]
pub fn discover_linux() -> Result<LinuxTopology, &'static str> {
    use std::fs;

    let sched_allowed = sched_allowed_cpus().ok_or("sched_getaffinity unavailable")?;
    let online = fs::read_to_string("/sys/devices/system/cpu/online")
        .ok()
        .and_then(|s| parse_cpu_list(&s))
        .unwrap_or_else(|| sched_allowed.clone());
    let allowed: Vec<_> = sched_allowed.into_iter().filter(|cpu| online.contains(cpu)).collect();
    let mut topology = HashMap::new();
    for &cpu in &allowed {
        if !online.contains(&cpu) {
            continue;
        }
        let root = format!("/sys/devices/system/cpu/cpu{cpu}/topology");
        let package = fs::read_to_string(format!("{root}/physical_package_id"))
            .ok()
            .and_then(|s| s.trim().parse().ok());
        let core = fs::read_to_string(format!("{root}/core_id"))
            .ok()
            .and_then(|s| s.trim().parse().ok());
        if let (Some(package), Some(core)) = (package, core) {
            topology.insert(cpu, CpuTopology { package, core });
        }
    }
    let ordered = ordered_allowed_cpus(&allowed, &online, &topology);
    let note = if ordered.is_some() {
        "sysfs topology available"
    } else if allowed.is_empty() {
        "no allowed online CPUs"
    } else {
        "sysfs topology unavailable"
    };
    Ok(LinuxTopology { allowed, ordered, note })
}

#[cfg(target_os = "linux")]
fn sched_allowed_cpus() -> Option<Vec<usize>> {
    let mut set = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    let result = unsafe { libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) };
    if result != 0 {
        return None;
    }
    let mut cpus = Vec::new();
    for cpu in 0..libc::CPU_SETSIZE as usize {
        if unsafe { libc::CPU_ISSET(cpu, &set) } {
            cpus.push(cpu);
        }
    }
    (!cpus.is_empty()).then_some(cpus)
}

#[cfg(target_os = "linux")]
fn mask_for(cpus: &[usize]) -> Option<libc::cpu_set_t> {
    if cpus.iter().any(|&cpu| cpu >= libc::CPU_SETSIZE as usize) {
        return None;
    }
    let mut set = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    unsafe {
        libc::CPU_ZERO(&mut set);
        for &cpu in cpus {
            libc::CPU_SET(cpu, &mut set);
        }
    }
    Some(set)
}

#[cfg(target_os = "linux")]
fn set_current_affinity(cpu: usize) -> bool {
    let Some(set) = mask_for(&[cpu]) else {
        return false;
    };
    unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0 }
}

#[cfg(target_os = "linux")]
pub fn can_control_affinity(cpu: usize) -> bool {
    // Probe on a short-lived thread so even an unexpected restore failure cannot
    // strand the main thread on one CPU.
    std::thread::Builder::new()
        .name("affinity-probe".into())
        .spawn(move || set_current_affinity(cpu))
        .ok()
        .and_then(|thread| thread.join().ok())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
pub fn pin_current_thread(cpu: usize) -> bool {
    set_current_affinity(cpu)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpuset_list_parses_and_deduplicates() {
        assert_eq!(parse_cpu_list("0,2-5,3,8-9"), Some(vec![0, 2, 3, 4, 5, 8, 9]));
        assert_eq!(parse_cpu_list("4-2"), None);
        assert_eq!(parse_cpu_list("1,,3"), None);
    }

    #[test]
    fn process_cpuset_orders_distinct_physical_cores_before_siblings() {
        let allowed = parse_cpu_list("0,2-5,8-9").unwrap();
        let online = parse_cpu_list("0,2-3,5,8-9").unwrap(); // CPU 4 is offline.
        let topology = HashMap::from([
            (0, CpuTopology { package: 0, core: 0 }),
            (2, CpuTopology { package: 0, core: 0 }),
            (3, CpuTopology { package: 0, core: 1 }),
            (4, CpuTopology { package: 0, core: 1 }),
            (5, CpuTopology { package: 1, core: 0 }),
            (8, CpuTopology { package: 1, core: 0 }),
            (9, CpuTopology { package: 1, core: 1 }),
        ]);
        assert_eq!(
            ordered_allowed_cpus(&allowed, &online, &topology),
            Some(vec![0, 3, 5, 9, 2, 8])
        );
        assert_eq!(workers_to_pin(&[0, 3, 5, 9, 2, 8], 4, true), Some(vec![0, 3, 5, 9]));
    }

    #[test]
    fn missing_topology_or_denied_control_falls_back() {
        let allowed = parse_cpu_list("1-2").unwrap();
        let online = allowed.clone();
        assert_eq!(ordered_allowed_cpus(&allowed, &online, &HashMap::new()), None);
        assert_eq!(workers_to_pin(&[1, 2], 2, false), None);
        assert_eq!(workers_to_pin(&[1, 2], 3, true), None);
    }
}
