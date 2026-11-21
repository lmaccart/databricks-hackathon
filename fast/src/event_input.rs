//! Read exact legacy chunk ranges without holding a copy of the whole input.
//! Use mmap for a file whose header or boundary cannot be found with a
//! bounded probe, so a single huge physical line never grows a read buffer.
use super::*;
use csv::Header;
use memchr::memchr;
use std::cell::RefCell;
use std::io;
use std::os::unix::fs::FileExt;

const PROBE: usize = 1024;
type Block = (u64, usize);

thread_local! {
    static BUFFER: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

enum Source {
    Read(PathBuf, Header, Vec<Block>),
    Mapped(PathBuf, Mmap),
}

pub(super) struct Files(Vec<Source>);

pub(super) enum Job<'a> {
    Read(&'a Path, Header, Block),
    Mapped(Chunk<'a>),
}

impl Job<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Read(_, _, (_, len)) => *len,
            Self::Mapped(chunk) => chunk.data.len(),
        }
    }

    pub fn with_block<T>(&self, f: impl FnOnce(Chunk<'_>) -> T) -> T {
        match *self {
            Self::Mapped(chunk) => f(chunk),
            Self::Read(path, header, (start, len)) => BUFFER.with_borrow_mut(|buf| {
                // Do not retain one descriptor per input part. Only active
                // workers open files, even when there are many small parts.
                let file = File::open(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                if buf.is_empty() {
                    buf.resize(CHUNK + PROBE, 0);
                }
                let bytes = &mut buf[..len];
                if file.read_exact_at(bytes, start).is_ok() {
                    f(header.chunk(bytes))
                } else {
                    // A read failure cannot send a partial row to the decoder.
                    // The mount is immutable, so the planned range still applies.
                    let mapped = unsafe { Mmap::map(&file).expect("mmap event read fallback") };
                    let start = usize::try_from(start).expect("mmap fallback offset");
                    f(header.chunk(&mapped[start..start + len]))
                }
            }),
        }
    }
}

/// Match chunks_of: start at the body, and extend from start + CHUNK to
/// just past the next physical LF (even if start + CHUNK is itself an LF).
fn plan(file: &File, len: u64, cols: &[&str], path: &Path) -> io::Result<Option<(Header, Vec<Block>)>> {
    let mut probe = [0; PROBE];
    let n = len.min(PROBE as u64) as usize;
    file.read_exact_at(&mut probe[..n], 0)?;
    let head = match memchr(b'\n', &probe[..n]) {
        Some(i) => i,
        None if n as u64 == len => n,
        None => return Ok(None),
    };
    let header = csv::header(&probe[..head], cols, path);
    let mut start = ((head + 1) as u64).min(len);
    let mut blocks = Vec::new();
    while start < len {
        let cut = start.saturating_add(CHUNK as u64).min(len);
        let n = (len - cut).min(PROBE as u64) as usize;
        let end = if n == 0 {
            len
        } else {
            file.read_exact_at(&mut probe[..n], cut)?;
            match memchr(b'\n', &probe[..n]) {
                Some(i) => cut + i as u64 + 1,
                None if cut + n as u64 == len => len,
                None => return Ok(None),
            }
        };
        blocks.push((start, (end - start) as usize));
        start = end;
    }
    Ok(Some((header, blocks)))
}

impl Files {
    pub fn open(dir: &Path, cols: &[&str]) -> Self {
        // Rayon preserves the input enumeration; the stable size sort below
        // therefore has the same tie order as the original mmap path.
        Self(
            csv_files(dir)
                .into_par_iter()
                .map(|(path, len)| {
                    let file = File::open(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                    if let Ok(Some((header, blocks))) = plan(&file, len, cols, &path) {
                        Source::Read(path, header, blocks)
                    } else {
                        let mapped = unsafe { Mmap::map(&file).expect("mmap event fallback") };
                        Source::Mapped(path, mapped)
                    }
                })
                .collect(),
        )
    }

    pub fn jobs(&self, cols: &[&str]) -> Vec<Job<'_>> {
        let mut jobs = Vec::new();
        for source in &self.0 {
            match source {
                Source::Read(path, header, blocks) => {
                    jobs.extend(blocks.iter().map(|&block| Job::Read(path, *header, block)));
                }
                Source::Mapped(path, bytes) => {
                    jobs.extend(chunks_of(bytes, cols, path).into_iter().map(Job::Mapped));
                }
            }
        }
        jobs.sort_by_key(|c| std::cmp::Reverse(c.len()));
        jobs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn positioned_blocks_are_exact_mmap_chunks_or_fall_back() {
        let path = std::env::temp_dir().join(format!("ss-pread-blocks-{}", std::process::id()));
        let mut file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        let cols = ["price_cents", "ts"];
        let hdr = b"\"ts\",price_cents\n";
        let mut data = hdr.to_vec();
        // Put an empty physical line right on the cut, then malformed quoted
        // content containing a physical newline and an unterminated final row.
        data.extend(std::iter::repeat_n(b'x', CHUNK - 1));
        data.extend_from_slice(b"\n\n\"bad\nr,1");
        file.write_all(&data).unwrap();
        let (header, blocks) = plan(&file, data.len() as u64, &cols, &path).unwrap().unwrap();
        let expected = chunks_of(&data, &cols, &path);
        assert_eq!(blocks.len(), expected.len());
        for ((start, len), expected) in blocks.into_iter().zip(expected) {
            let mut buf = vec![0; len];
            file.read_exact_at(&mut buf, start).unwrap();
            let got = header.chunk(&buf);
            assert_eq!(got.data, expected.data);
            let mut want = Vec::new();
            let mut rows = Vec::new();
            for_rows(&expected, |r| want.push(r.to_vec()));
            for_rows(&got, |r| rows.push(r.to_vec()));
            assert_eq!(rows, want);
        }
        for header_len in [hdr.len() - 1, hdr.len()] {
            assert!(
                plan(&file, header_len as u64, &cols, &path)
                    .unwrap()
                    .unwrap()
                    .1
                    .is_empty()
            );
        }
        file.write_all_at(&[b'x'; PROBE + 1], (hdr.len() + CHUNK) as u64)
            .unwrap();
        assert!(
            plan(&file, file.metadata().unwrap().len(), &cols, &path)
                .unwrap()
                .is_none()
        );
    }
}
