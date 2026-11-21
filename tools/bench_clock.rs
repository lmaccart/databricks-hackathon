//! Mixed F1/F2/F3 parser-only comparison; does not measure pipeline startup.
//! Run: rustc -O --edition 2024 tools/bench_clock.rs -o data/bench_clock
//!      data/bench_clock
#[allow(dead_code)]
#[path = "../fast/src/ts.rs"]
mod ts;
const BAD: i64 = i64::MIN;
use std::hint::black_box;
use std::time::Instant;

fn main() {
    let clock = ts::ManifestClock::new(
        ts::parse_ts(b"2028-02-20T00:00:00Z"), ts::parse_ts(b"2028-03-05T00:00:00Z"));
    let mut rng = 123456789u64;
    let mut inputs = Vec::new();
    for _ in 0..32768 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let day = 20 + rng % 10;
        let hour = (rng >> 8) % 24;
        let minute = (rng >> 16) % 60;
        let second = (rng >> 24) % 60;
        let iso = format!("2028-02-{day:02}T{hour:02}:{minute:02}:{second:02}Z");
        let input = if rng % 10 < 3 {
            ts::parse_ts(iso.as_bytes()).to_string()
        } else if rng % 10 == 3 {
            format!("2028-02-{day:02}T{hour:02}:{minute:02}:{second:02}+00:00")
        } else {
            iso
        };
        inputs.push(input);
    }
    for i in 0..12 {
        for side in if i % 2 == 0 { [0, 1] } else { [1, 0] } {
            let start = Instant::now();
            let mut sum = 0i64;
            for _ in 0..150 {
                for input in &inputs {
                    let s = black_box(input.as_bytes());
                    sum = sum.wrapping_add(if side == 0 { ts::parse_ts(s) } else { clock.parse_ts(s) });
                }
            }
            println!("{i} {side} {:.6} {}", start.elapsed().as_secs_f64(), black_box(sum));
        }
    }
}
