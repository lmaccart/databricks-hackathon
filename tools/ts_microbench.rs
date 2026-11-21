// Standalone parser microbenchmark. See docs/NOTES.md for reproducible commands.
// TS_SOURCE must point to a copy of ts.rs without its leading //! documentation.
const BAD: i64 = i64::MIN;
mod ts {
    include!(env!("TS_SOURCE"));
}

use std::{hint::black_box, time::Instant};

fn main() {
    let mut cases: [Vec<Vec<u8>>; 4] = Default::default();
    for i in 0..65_536 {
        let year = 1990 + i % 80;
        let month = 1 + (i / 80) % 12;
        let day = 1 + (i / 960) % 28;
        let hour = i % 24;
        let minute = (i / 24) % 60;
        let second = (i / 97) % 60;
        let f1 = format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
            .into_bytes();
        let f2 = format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}-05:30")
            .into_bytes();
        let f3 = format!("{}", 1_700_000_000_000u64 + i as u64 * 1_234_567).into_bytes();
        cases[3].push(match i % 100 {
            0..=57 => f1.clone(),
            58..=67 => f2.clone(),
            _ => f3.clone(),
        });
        cases[0].push(f1);
        cases[1].push(f2);
        cases[2].push(f3);
    }
    for (name, values) in ["F1", "F2", "F3-13", "mixed"].iter().zip(&cases) {
        let mut samples = Vec::new();
        let mut checksum = 0i64;
        for _ in 0..5 {
            let start = Instant::now();
            for _ in 0..32 {
                for s in values {
                    checksum = checksum.wrapping_add(black_box(ts::parse_ts(black_box(s))));
                }
            }
            samples.push(start.elapsed().as_secs_f64() * 1e9 / (values.len() * 32) as f64);
        }
        samples.sort_by(f64::total_cmp);
        println!(
            "{name}: {:.2} ns/value (median of 5), checksum {}",
            samples[2],
            black_box(checksum)
        );
    }
}
