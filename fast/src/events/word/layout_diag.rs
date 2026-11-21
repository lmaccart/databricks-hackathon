use super::*;
use crate::events::Pending;
use crate::ts;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const BLOCK_BYTES: usize = 32 * 1024;
const LANES: usize = 8;
const PAIRS: usize = 7;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RowResult {
    event: Option<Pending>,
    fallback: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct Summary {
    rows: u64,
    decoded: u64,
    fallbacks: u64,
    digest: u64,
}

impl Summary {
    #[inline]
    fn push(&mut self, row: RowResult) {
        self.rows += 1;
        self.fallbacks += u64::from(row.fallback);
        if let Some(e) = row.event {
            self.decoded += 1;
            self.digest = self.digest.rotate_left(9) ^ e.claim_key;
            self.digest = self.digest.rotate_left(7) ^ e.uid;
            self.digest = self.digest.rotate_left(11) ^ e.hash;
            self.digest = self.digest.rotate_left(13) ^ e.ts as u64;
            self.digest = self.digest.rotate_left(5) ^ e.claim_order;
            self.digest = self.digest.rotate_left(3) ^ u64::from(e.price);
            self.digest = self.digest.rotate_left(17) ^ u64::from(e.cat);
        } else {
            self.digest = self.digest.rotate_left(3) ^ 0x9e37_79b9_7f4a_7c15;
        }
        self.digest = self.digest.rotate_left(1) ^ u64::from(row.fallback);
    }

    fn merge(&mut self, other: Self) {
        self.rows += other.rows;
        self.decoded += other.decoded;
        self.fallbacks += other.fallbacks;
        // The final digest is only a work checksum across chunks; exact equality
        // is checked row-by-row in the untimed correctness pass.
        self.digest = self.digest.rotate_left(17) ^ other.digest ^ other.rows;
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct Prefix {
    event_id: u8,
    user_id: u8,
    event_type: u8,
}

struct Row<'a> {
    tail: &'a [u8],
    fields: Option<[&'a [u8]; 7]>,
    prefix: Option<Prefix>,
}

#[inline]
fn baseline_one<'a>(
    input: &'a [u8],
    sm: &Small,
    codec: &Codec<'a>,
    window: (i64, i64, &ManifestClock),
    spill: &mut Vec<i64>,
) -> (RowResult, &'a [u8]) {
    if let Some((event, rest)) = decode(input, sm, window) {
        return (
            RowResult {
                event: Some(event),
                fallback: false,
            },
            rest,
        );
    }
    let (event, rest) = fallback(input, sm, codec, window, spill);
    (RowResult { event, fallback: true }, rest)
}

fn baseline_rows<'a>(
    c: &Chunk<'a>,
    sm: &Small,
    codec: &Codec<'a>,
    window: (i64, i64, &ManifestClock),
) -> Vec<RowResult> {
    assert!(c.identity_order(7), "diagnostic baseline requires canonical headers");
    let mut out = Vec::new();
    let mut spill = Vec::new();
    let mut rest = c.data;
    while !rest.is_empty() {
        let (row, tail) = baseline_one(rest, sm, codec, window, &mut spill);
        out.push(row);
        rest = tail;
    }
    out
}

fn baseline_summary<'a>(c: &Chunk<'a>, sm: &Small, codec: &Codec<'a>, window: (i64, i64, &ManifestClock)) -> Summary {
    assert!(c.identity_order(7), "diagnostic baseline requires canonical headers");
    let mut out = Summary::default();
    let mut spill = Vec::new();
    let mut rest = c.data;
    while !rest.is_empty() {
        let (row, tail) = baseline_one(rest, sm, codec, window, &mut spill);
        out.push(row);
        rest = tail;
    }
    out
}

#[inline]
fn fields_exact(line: &[u8]) -> Option<([&[u8]; 7], [bool; 7])> {
    let body = line.strip_suffix(b"\n").unwrap_or(line);
    if body.is_empty() {
        return None;
    }
    let mut fields = [&[][..]; 7];
    let mut quoted = [false; 7];
    let mut rest = body;
    for i in 0..7 {
        let (raw, tail) = if i == 6 {
            if memchr::memchr(b',', rest).is_some() {
                return None;
            }
            (rest, &rest[rest.len()..])
        } else {
            let comma = memchr::memchr(b',', rest)?;
            (&rest[..comma], &rest[comma + 1..])
        };
        if memchr::memchr(b'"', raw).is_some() {
            if raw.len() < 2
                || raw[0] != b'"'
                || raw[raw.len() - 1] != b'"'
                || memchr::memchr(b'"', &raw[1..raw.len() - 1]).is_some()
            {
                return None;
            }
            fields[i] = &raw[1..raw.len() - 1];
            quoted[i] = true;
        } else {
            fields[i] = raw;
        }
        rest = tail;
    }
    (rest.is_empty()).then_some((fields, quoted))
}

#[inline]
fn id_word_ok(s: &[u8]) -> bool {
    (7..=9).contains(&s.len()) && s.iter().all(|&b| (b'-'..=127).contains(&b))
}

#[inline]
fn eligible(line: &[u8], fields: &[&[u8]; 7], quoted: &[bool; 7]) -> Option<Prefix> {
    if line.last() != Some(&b'\n')
        || !id_word_ok(fields[0])
        || !id_word_ok(fields[1])
        || quoted[..5].iter().any(|&q| q)
        || fields[2].len() != 20
        || fields[3].len() != 20
        || fields[2].get(4) != Some(&b'-')
        || !matches!(fields[4], b"view" | b"cart" | b"purchase")
    {
        return None;
    }
    let suffix_offset = fields[5].as_ptr() as usize - line.as_ptr() as usize - usize::from(quoted[5]);
    if line.len().saturating_sub(suffix_offset) < 16 {
        return None;
    }
    Some(Prefix {
        event_id: fields[0].len() as u8,
        user_id: fields[1].len() as u8,
        event_type: fields[4].len() as u8,
    })
}

#[inline]
fn id_key<'a>(s: &'a [u8], codec: &Codec<'a>) -> Option<u64> {
    if !id_word_ok(s) {
        return None;
    }
    person::inline(s).or_else(|| Some(codec.key(s)))
}

#[inline]
fn decoded_fields<'a>(
    f: &[&'a [u8]; 7],
    ts_ms: i64,
    ingest_seconds: i64,
    sm: &Small,
    codec: &Codec<'a>,
    (ws, we, _): (i64, i64, &ManifestClock),
) -> Option<Pending> {
    let eid = id_key(f[0], codec)?;
    let uid = id_key(f[1], codec)?;
    let purchase = f[4] == b"purchase";
    let payload = if f[5].is_empty() {
        None
    } else if purchase {
        parse_price(f[6]).map(|price| (price, sm.product_cat.get(f[5]).unwrap_or(sm.unknown_cat)))
    } else {
        Some((0, 0))
    };
    let mut event = Pending {
        claim_key: links::mix(eid),
        ts: ts_ms,
        claim_order: ingest_seconds as u64,
        ..Default::default()
    };
    // Keep the parser's tentative-payload semantics: an invalid row still
    // emits its event-ID claim and can suppress a later valid copy.
    if (ws..we).contains(&ts_ms)
        && ingest_seconds * 1000 - ts_ms <= DAY
        && let Some((price, cat)) = payload
    {
        event.uid = uid;
        event.hash = links::mix(uid);
        event.price = price;
        event.cat = cat;
    }
    Some(event)
}

fn block_rows<'a>(data: &'a [u8], full: &'a [u8], absolute: usize) -> Vec<Row<'a>> {
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < data.len() {
        let end = memchr::memchr(b'\n', &data[start..]).map_or(data.len(), |n| start + n + 1);
        let line = &data[start..end];
        let parsed = fields_exact(line);
        let fields = parsed.map(|x| x.0);
        let quoted = parsed.map_or([false; 7], |x| x.1);
        let prefix = fields.as_ref().and_then(|f| eligible(line, f, &quoted));
        out.push(Row {
            tail: &full[absolute + start..],
            fields,
            prefix,
        });
        start = end;
    }
    out
}

fn candidate_block<'a>(
    rows: &[Row<'a>],
    sm: &Small,
    codec: &Codec<'a>,
    window: (i64, i64, &ManifestClock),
    avx2: bool,
) -> Vec<RowResult> {
    let mut out = vec![RowResult::default(); rows.len()];
    let mut groups: HashMap<Prefix, Vec<usize>> = HashMap::new();
    let mut spill = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if let Some(prefix) = row.prefix {
            groups.entry(prefix).or_default().push(i);
        } else {
            let (result, _) = baseline_one(row.tail, sm, codec, window, &mut spill);
            out[i] = result;
        }
    }
    for indexes in groups.values() {
        for batch in indexes.chunks_exact(LANES) {
            let events: [&[u8]; LANES] = std::array::from_fn(|i| rows[batch[i]].fields.unwrap()[2]);
            let ingests: [&[u8]; LANES] = std::array::from_fn(|i| rows[batch[i]].fields.unwrap()[3]);
            let event_sod = ts::f1_seconds_of_day8(events, avx2);
            let ingest_sod = ts::f1_seconds_of_day8(ingests, avx2);
            for lane in 0..LANES {
                let index = batch[lane];
                let f = rows[index].fields.unwrap();
                let event_seconds = window
                    .2
                    .event_f1_seconds_of_day(f[2].first_chunk::<20>().unwrap(), event_sod[lane]);
                let ingest_seconds = window
                    .2
                    .event_f1_seconds_of_day(f[3].first_chunk::<20>().unwrap(), ingest_sod[lane]);
                let fast = if event_seconds == BAD || ingest_seconds == BAD {
                    None
                } else {
                    decoded_fields(&f, event_seconds * 1000, ingest_seconds, sm, codec, window)
                };
                out[index] = if let Some(event) = fast {
                    RowResult {
                        event: Some(event),
                        fallback: false,
                    }
                } else {
                    baseline_one(rows[index].tail, sm, codec, window, &mut spill).0
                };
            }
        }
        for &index in indexes.iter().skip(indexes.len() / LANES * LANES) {
            let (result, _) = baseline_one(rows[index].tail, sm, codec, window, &mut spill);
            out[index] = result;
        }
    }
    out
}

fn candidate_rows<'a>(
    c: &Chunk<'a>,
    sm: &Small,
    codec: &Codec<'a>,
    window: (i64, i64, &ManifestClock),
    avx2: bool,
) -> Vec<RowResult> {
    assert!(c.identity_order(7), "diagnostic candidate requires canonical headers");
    let mut output = Vec::new();
    let mut offset = 0usize;
    while offset < c.data.len() {
        let nominal = (offset + BLOCK_BYTES).min(c.data.len());
        let end = if nominal < c.data.len() {
            memchr::memchr(b'\n', &c.data[nominal..]).map_or(c.data.len(), |n| nominal + n + 1)
        } else {
            c.data.len()
        };
        let rows = block_rows(&c.data[offset..end], c.data, offset);
        output.extend(candidate_block(&rows, sm, codec, window, avx2));
        offset = end;
    }
    output
}

fn candidate_summary<'a>(
    c: &Chunk<'a>,
    sm: &Small,
    codec: &Codec<'a>,
    window: (i64, i64, &ManifestClock),
    avx2: bool,
) -> Summary {
    assert!(c.identity_order(7), "diagnostic candidate requires canonical headers");
    let mut output = Summary::default();
    let mut offset = 0usize;
    while offset < c.data.len() {
        let nominal = (offset + BLOCK_BYTES).min(c.data.len());
        let end = if nominal < c.data.len() {
            memchr::memchr(b'\n', &c.data[nominal..]).map_or(c.data.len(), |n| nominal + n + 1)
        } else {
            c.data.len()
        };
        let rows = block_rows(&c.data[offset..end], c.data, offset);
        for row in candidate_block(&rows, sm, codec, window, avx2) {
            output.push(row);
        }
        offset = end;
    }
    output
}

fn timed_baseline<'a>(
    chunks: &[Chunk<'a>],
    sm: &Small,
    codec: &Codec<'a>,
    window: (i64, i64, &ManifestClock),
) -> (Duration, Summary) {
    let start = Instant::now();
    let parts: Vec<_> = chunks
        .par_iter()
        .map(|c| baseline_summary(c, sm, codec, window))
        .collect();
    let elapsed = start.elapsed();
    (
        elapsed,
        parts.into_iter().fold(Summary::default(), |mut a, b| {
            a.merge(b);
            a
        }),
    )
}

fn timed_candidate<'a>(
    chunks: &[Chunk<'a>],
    sm: &Small,
    codec: &Codec<'a>,
    window: (i64, i64, &ManifestClock),
    avx2: bool,
) -> (Duration, Summary) {
    let start = Instant::now();
    let parts: Vec<_> = chunks
        .par_iter()
        .map(|c| candidate_summary(c, sm, codec, window, avx2))
        .collect();
    let elapsed = start.elapsed();
    (
        elapsed,
        parts.into_iter().fold(Summary::default(), |mut a, b| {
            a.merge(b);
            a
        }),
    )
}

fn verify<'a>(
    chunks: &[Chunk<'a>],
    sm: &Small,
    codec: &Codec<'a>,
    window: (i64, i64, &ManifestClock),
    avx2: bool,
) -> Summary {
    let mut total = Summary::default();
    for (ci, c) in chunks.iter().enumerate() {
        let baseline = baseline_rows(c, sm, codec, window);
        let candidate = candidate_rows(c, sm, codec, window, avx2);
        assert_eq!(
            candidate.len(),
            baseline.len(),
            "physical-row count differs in chunk {ci}"
        );
        for (ri, (want, got)) in baseline.into_iter().zip(candidate).enumerate() {
            assert_eq!(
                got, want,
                "decoded fields/fallback differ at chunk {ci}, row {ri}: {:?}",
                c.data
            );
            total.push(got);
        }
    }
    total
}

pub(in crate::events) fn run(inputs: Vec<PathBuf>) {
    assert_eq!(inputs.len(), 2, "pass exactly two 20M input directories");
    #[cfg(target_arch = "x86_64")]
    let avx2 = std::arch::is_x86_feature_detected!("avx2");
    #[cfg(not(target_arch = "x86_64"))]
    let avx2 = false;
    eprintln!(
        "layout-simd diagnostic: AVX2={avx2}, Rayon workers={}",
        rayon::current_num_threads()
    );
    for (dataset, input) in inputs.iter().enumerate() {
        run_dataset(input, dataset == 1, avx2);
    }
}

fn run_dataset(input: &PathBuf, reverse_first: bool, avx2: bool) {
    use memmap2::Mmap;
    use std::fs::File;

    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(input.join("manifest.json")).expect("manifest")).expect("JSON");
    let ws = parse_ts(manifest["window_start"].as_str().unwrap().as_bytes());
    let we = parse_ts(manifest["window_end"].as_str().unwrap().as_bytes());
    let clock = ManifestClock::new(ws, we);
    let product_file = File::open(input.join("products.csv")).expect("products.csv");
    let campaign_file = File::open(input.join("campaigns.csv")).expect("campaigns.csv");
    // SAFETY: the diagnostic reads these immutable input files for their full lifetime.
    let products = unsafe { Mmap::map(&product_file).expect("mmap products") };
    // SAFETY: the diagnostic reads these immutable input files for their full lifetime.
    let campaigns = unsafe { Mmap::map(&campaign_file).expect("mmap campaigns") };
    let sm = crate::read_small(
        &products,
        &campaigns,
        &input.join("products.csv"),
        &input.join("campaigns.csv"),
    );
    let files = crate::mmap_files(&input.join("events"));
    let columns = [
        "event_id",
        "user_id",
        "ts",
        "ingest_ts",
        "event_type",
        "product_id",
        "price_cents",
    ];
    let chunks = crate::chunks_for(&files, &columns);
    assert!(!chunks.is_empty(), "no event chunks found in {}", input.display());
    assert!(
        chunks.iter().all(|c| c.identity_order(7)),
        "requires canonical event headers"
    );
    let codec = Codec::new();
    let window = (ws, we, &clock);
    let exact = verify(&chunks, &sm, &codec, window, avx2);
    eprintln!(
        "{}: verified {} physical rows, {} decoded, {} fallbacks, digest={:016x}",
        input.display(),
        exact.rows,
        exact.decoded,
        exact.fallbacks,
        exact.digest
    );

    // Warm both paths before timing, in opposite order on the second seed.
    if reverse_first {
        let _ = timed_candidate(&chunks, &sm, &codec, window, avx2);
        let _ = timed_baseline(&chunks, &sm, &codec, window);
    } else {
        let _ = timed_baseline(&chunks, &sm, &codec, window);
        let _ = timed_candidate(&chunks, &sm, &codec, window, avx2);
    }

    let (mut baseline_times, mut candidate_times) = (Vec::new(), Vec::new());
    for pair in 0..PAIRS {
        let candidate_first = reverse_first ^ (pair % 2 == 1);
        let (b, c) = if candidate_first {
            let c = timed_candidate(&chunks, &sm, &codec, window, avx2);
            let b = timed_baseline(&chunks, &sm, &codec, window);
            (b, c)
        } else {
            let b = timed_baseline(&chunks, &sm, &codec, window);
            let c = timed_candidate(&chunks, &sm, &codec, window, avx2);
            (b, c)
        };
        assert_eq!(
            (b.1.rows, b.1.decoded, b.1.fallbacks),
            (exact.rows, exact.decoded, exact.fallbacks)
        );
        assert_eq!(
            (c.1.rows, c.1.decoded, c.1.fallbacks),
            (exact.rows, exact.decoded, exact.fallbacks)
        );
        assert_eq!(b.1.digest, c.1.digest, "timed path checksums differ");
        eprintln!(
            "pair {} order={} baseline={:.6}s candidate={:.6}s speedup={:.3}x",
            pair + 1,
            if candidate_first { "CB" } else { "BC" },
            b.0.as_secs_f64(),
            c.0.as_secs_f64(),
            b.0.as_secs_f64() / c.0.as_secs_f64()
        );
        baseline_times.push(b.0);
        candidate_times.push(c.0);
    }
    baseline_times.sort();
    candidate_times.sort();
    let baseline_median = baseline_times[PAIRS / 2].as_secs_f64();
    let candidate_median = candidate_times[PAIRS / 2].as_secs_f64();
    eprintln!(
        "MEDIAN {} baseline={baseline_median:.6}s candidate={candidate_median:.6}s speedup={:.3}x screen={}",
        input.display(),
        baseline_median / candidate_median,
        if baseline_median / candidate_median >= 1.30 {
            "PASS"
        } else {
            "FAIL"
        }
    );
}
