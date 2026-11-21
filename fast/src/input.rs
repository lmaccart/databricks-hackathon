//! Optional, bounded anonymous input buffers. All other inputs remain file mappings.

use memmap2::{Mmap, MmapMut};
use rayon::prelude::*;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

// Bound both the copy in anonymous memory and the file cache it can induce.
const CAP: usize = 512 << 20;
const MIN_FILE: usize = 16 << 20;
const READ_PART: usize = 2 << 20;

#[derive(Default)]
pub struct Inputs {
    enabled: bool,
    diagnostic: bool,
    retained: usize,
    files: usize,
    failures: usize,
}

impl Inputs {
    pub fn new() -> Self {
        Self {
            enabled: cfg!(target_os = "linux") && std::env::var_os("SS_INPUT_VM").as_deref() != Some("stock".as_ref()),
            diagnostic: std::env::var_os("SS_VM_DIAG").as_deref() == Some("1".as_ref()),
            ..Self::default()
        }
    }

    pub fn files(&mut self, dir: &Path) -> Vec<(PathBuf, Mmap)> {
        let rd = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        let mut paths = Vec::new();
        for entry in rd {
            let p = entry.expect("read directory entry").path();
            if p.extension().is_some_and(|x| x == "csv") {
                let meta = p.metadata().unwrap_or_else(|e| panic!("{}: {e}", p.display()));
                if meta.is_file() && meta.len() != 0 {
                    paths.push((p, meta.len()));
                }
            }
        }
        // Spend the finite budget on the largest eligible files first.
        if self.enabled {
            paths.sort_unstable_by_key(|(_, len)| std::cmp::Reverse(*len));
        }
        paths
            .into_iter()
            .map(|(p, len)| {
                let f = File::open(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
                let size = usize::try_from(len).unwrap_or(usize::MAX);
                let anonymous = if self.enabled && eligible(size, self.retained) {
                    match read_anonymous(&f, size) {
                        Ok(m) => {
                            self.retained += size;
                            self.files += 1;
                            Some(m)
                        }
                        Err(_) => {
                            self.failures += 1;
                            None
                        }
                    }
                } else {
                    None
                };
                let m = anonymous.unwrap_or_else(|| unsafe { Mmap::map(&f).expect("mmap") });
                (p, m)
            })
            .collect()
    }

    pub fn report(&self, phase: &str) {
        if !self.diagnostic {
            return;
        }
        #[cfg(target_os = "linux")]
        {
            // RUSAGE_SELF includes all threads; these counters are cumulative.
            let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
            let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
            if rc == 0 {
                let usage = unsafe { usage.assume_init() };
                eprintln!(
                    "[vm] {phase} mode={} anon_files={} anon_bytes={} fallback_failures={} minor={} major={} maxrss_kib={}",
                    if self.enabled { "huge" } else { "mmap" },
                    self.files,
                    self.retained,
                    self.failures,
                    usage.ru_minflt,
                    usage.ru_majflt,
                    usage.ru_maxrss
                );
            }
        }
        #[cfg(not(target_os = "linux"))]
        eprintln!("[vm] {phase} Linux input VM path unavailable; mode=mmap");
    }
}

fn eligible(size: usize, retained: usize) -> bool {
    size >= MIN_FILE && size <= CAP.saturating_sub(retained)
}

fn read_anonymous(file: &File, size: usize) -> io::Result<Mmap> {
    let mut mapping = MmapMut::map_anon(size)?;
    #[cfg(target_os = "linux")]
    {
        // Advice is only a hint: THP may be disabled, fragmented, or denied.
        // On an unsupported or filtered kernel, keep the original mmap path.
        let rc = unsafe { libc::madvise(mapping.as_mut_ptr().cast(), size, libc::MADV_HUGEPAGE) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    read_into_mapping(file, &mut mapping)?;
    mapping.make_read_only()
}

fn read_into_mapping(file: &File, mapping: &mut MmapMut) -> io::Result<()> {
    mapping
        .par_chunks_mut(READ_PART)
        .enumerate()
        .try_for_each(|(i, buf)| file.read_exact_at(buf, (i * READ_PART) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn budget_rejects_small_and_overlarge_files() {
        assert!(!eligible(MIN_FILE - 1, 0));
        assert!(eligible(MIN_FILE, CAP - MIN_FILE));
        assert!(!eligible(MIN_FILE, CAP - MIN_FILE + 1));
        assert!(!eligible(usize::MAX, 0));
        assert!(!eligible(MIN_FILE, usize::MAX));
    }

    #[test]
    fn parallel_read_is_exact_and_short_read_fails() {
        let path = std::env::temp_dir().join(format!("ss-input-{}", std::process::id()));
        let mut file = File::create(&path).unwrap();
        let bytes: Vec<u8> = (0..READ_PART + 19).map(|i| (i % 251) as u8).collect();
        file.write_all(&bytes).unwrap();
        drop(file);
        let file = File::open(&path).unwrap();
        let mut mapping = MmapMut::map_anon(bytes.len()).unwrap();
        read_into_mapping(&file, &mut mapping).unwrap();
        assert_eq!(&mapping[..], &bytes);
        let mut too_long = MmapMut::map_anon(bytes.len() + 1).unwrap();
        assert!(read_into_mapping(&file, &mut too_long).is_err());
        // MADV_HUGEPAGE can be unavailable or blocked even on Linux.
        if let Ok(advised) = read_anonymous(&file, bytes.len()) {
            assert_eq!(&advised[..], &bytes);
        }
        std::fs::remove_file(path).unwrap();
    }
}
