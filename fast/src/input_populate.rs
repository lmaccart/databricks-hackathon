//! Bounded Linux page-table population for the existing read-only file maps.
//!
//! MADV_POPULATE_READ faults the mapped pages into this process; unlike
//! WILLNEED, it does not merely warm the file cache.

use memmap2::Mmap;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering::Relaxed};

#[cfg(target_os = "linux")]
const REGION_BYTES: usize = 16 << 20;
#[cfg(target_os = "linux")]
const MAX_WORKERS: usize = 2;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Stats {
    pub(super) input_bytes: u64,
    pub(super) populated_bytes: u64,
    pub(super) workers: usize,
    pub(super) error: Option<i32>,
    pub(super) minor_faults: Option<i64>,
    pub(super) major_faults: Option<i64>,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct Region<'a> {
    map: &'a Mmap,
    start: usize,
    len: usize,
}

#[cfg(target_os = "linux")]
fn regions<'a>(maps: &[&'a Mmap]) -> Vec<Region<'a>> {
    let mut out = Vec::new();
    for map in maps {
        let mut start = 0;
        while start < map.len() {
            let len = (map.len() - start).min(REGION_BYTES);
            out.push(Region { map, start, len });
            start += len;
        }
    }
    out
}

/// Run `work` while a small, fixed share of the existing Rayon pool populates
/// input pages. Input size and region count come only from the mapped files.
pub(super) fn during<T: Send>(
    maps: &[&Mmap],
    work: impl FnOnce() -> T + Send,
    enabled: bool,
    diagnostics: bool,
) -> (T, Stats) {
    #[cfg(target_os = "linux")]
    {
        if enabled {
            return during_linux(maps, work, diagnostics);
        }
    }

    let _ = (maps, enabled, diagnostics);
    (work(), Stats::default())
}

#[cfg(target_os = "linux")]
fn during_linux<T: Send>(maps: &[&Mmap], work: impl FnOnce() -> T + Send, diagnostics: bool) -> (T, Stats) {
    let regions = regions(maps);
    let input_bytes = regions.iter().map(|r| r.len as u64).sum();
    let pool_workers = rayon::current_num_threads();
    // Leave most of the pool available to graph parsing and union work.
    let workers = MAX_WORKERS.min(pool_workers.saturating_sub(1)).min(regions.len());
    if workers == 0 {
        return (work(), Stats::default());
    }

    let before = diagnostics.then(get_usage).flatten();
    let next = AtomicUsize::new(0);
    let stopped = AtomicBool::new(false);
    let populated_bytes = AtomicU64::new(0);
    let error = AtomicI32::new(0);

    let result = rayon::scope(|scope| {
        for _ in 0..workers {
            let regions = &regions;
            let next = &next;
            let stopped = &stopped;
            let populated_bytes = &populated_bytes;
            let error = &error;
            scope.spawn(move |_| {
                while !stopped.load(Relaxed) {
                    let i = next.fetch_add(1, Relaxed);
                    let Some(region) = regions.get(i) else {
                        break;
                    };
                    // Region starts are page-aligned; the kernel rounds the
                    // final partial page in the same way as madvise.
                    let address = unsafe { region.map.as_ptr().add(region.start) };
                    let rc = unsafe { libc::madvise(address.cast_mut().cast(), region.len, MADV_POPULATE_READ) };
                    if rc == 0 {
                        populated_bytes.fetch_add(region.len as u64, Relaxed);
                    } else {
                        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO);
                        let _ = error.compare_exchange(0, errno, Relaxed, Relaxed);
                        // Includes unsupported kernels and resource limits.
                        // Already populated pages remain valid; demand paging
                        // handles every page we did not finish advising.
                        stopped.store(true, Relaxed);
                    }
                }
            });
        }
        work()
    });

    let after = diagnostics.then(get_usage).flatten();
    let stats = Stats {
        input_bytes,
        populated_bytes: populated_bytes.load(Relaxed),
        workers,
        error: match error.load(Relaxed) {
            0 => None,
            errno => Some(errno),
        },
        minor_faults: before.zip(after).map(|(a, b)| b.0 - a.0),
        major_faults: before.zip(after).map(|(a, b)| b.1 - a.1),
    };
    (result, stats)
}

#[cfg(target_os = "linux")]
const MADV_POPULATE_READ: libc::c_int = 22;

#[cfg(target_os = "linux")]
fn get_usage() -> Option<(i64, i64)> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    let usage = unsafe { usage.assume_init() };
    Some((usage.ru_minflt as i64, usage.ru_majflt as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use std::fs::File;
    #[cfg(target_os = "linux")]
    use std::fs::OpenOptions;
    #[cfg(target_os = "linux")]
    use std::io::{Read, Seek, SeekFrom, Write};
    #[cfg(target_os = "linux")]
    use std::sync::atomic::AtomicU64;

    #[test]
    fn disabled_mode_still_runs_work_without_population() {
        let (value, stats) = during(&[], || 7, false, false);
        assert_eq!(value, 7);
        assert_eq!(stats.input_bytes, 0);
        assert_eq!(stats.populated_bytes, 0);
        assert_eq!(stats.workers, 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn region_plan_covers_actual_map_lengths_without_oversized_regions() {
        let paths = [
            std::env::temp_dir().join(format!("populate-plan-{}-a", std::process::id())),
            std::env::temp_dir().join(format!("populate-plan-{}-b", std::process::id())),
        ];
        let mut maps = Vec::new();
        for (path, len) in paths.iter().zip([REGION_BYTES + 7, 23]) {
            let file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .read(true)
                .open(path)
                .unwrap();
            file.set_len(len as u64).unwrap();
            maps.push(unsafe { Mmap::map(&file).unwrap() });
        }
        let refs: Vec<_> = maps.iter().collect();
        let planned = regions(&refs);
        assert_eq!(planned.iter().map(|r| r.len).sum::<usize>(), REGION_BYTES + 30);
        assert!(planned.iter().all(|r| r.len <= REGION_BYTES));
        assert!(planned.iter().all(|r| r.start % REGION_BYTES == 0));
        drop(planned);
        drop(refs);
        drop(maps);
        for path in paths {
            std::fs::remove_file(path).unwrap();
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn populate_read_installs_present_file_backed_page_table_entries() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "populate-pte-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Relaxed)
        ));
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
        let page_count = 3;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .read(true)
            .open(&path)
            .unwrap();
        for i in 0..page_count {
            file.write_all(&vec![0x40 + i as u8; page_size]).unwrap();
        }
        file.sync_all().unwrap();
        let map = unsafe { Mmap::map(&file).unwrap() };

        let pool = rayon::ThreadPoolBuilder::new().num_threads(2).build().unwrap();
        let ((), stats) = pool.install(|| during(&[&map], || (), true, false));
        if stats
            .error
            .is_some_and(|e| e == libc::EINVAL || e == libc::ENOSYS || e == libc::EOPNOTSUPP)
        {
            drop(map);
            drop(file);
            std::fs::remove_file(path).unwrap();
            return; // Older Linux kernels do not implement the advice.
        }
        assert_eq!(stats.input_bytes, map.len() as u64);
        assert_eq!(stats.populated_bytes, map.len() as u64);
        assert_eq!(stats.workers, 1);

        let mut pagemap = File::open("/proc/self/pagemap").unwrap();
        for page in 0..page_count {
            let virtual_page = (map.as_ptr() as usize / page_size) + page;
            pagemap.seek(SeekFrom::Start((virtual_page * 8) as u64)).unwrap();
            let mut entry = [0; 8];
            pagemap.read_exact(&mut entry).unwrap();
            let entry = u64::from_ne_bytes(entry);
            assert_ne!(
                entry & (1 << 63),
                0,
                "page {page} has no present PTE after successful population"
            );
        }
        drop(map);
        drop(file);
        std::fs::remove_file(path).unwrap();
    }
}
