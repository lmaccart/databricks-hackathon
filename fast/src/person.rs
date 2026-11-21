//! An exact person key, cheap enough to carry through both sorts. Nine ASCII
//! bytes fit in 63 bits. Every other spelling uses a collision-checked interner;
//! no hash (or truncated hash) is ever accepted as proof of identity.
use crate::{Map, rs};
use std::sync::RwLock;

const HIGH_BITS: u64 = 0x8080_8080_8080_8080;
#[cfg(target_arch = "x86_64")]
const LOW_BITS: u64 = !HIGH_BITS;
const EXTERNAL: u64 = 1 << 63;
const LANES: usize = 64;

/// A zero-size capability for the specialized row/output loops. The true
/// variant is only constructed after checking the actual CPU.
#[derive(Clone, Copy)]
pub(crate) struct IdMode<const BMI2: bool>(());

impl IdMode<false> {
    pub(crate) const SCALAR: Self = Self(());
}

#[cfg(target_arch = "x86_64")]
impl IdMode<true> {
    pub(crate) fn detect() -> Option<Self> {
        use std::sync::OnceLock;
        static FAST: OnceLock<bool> = OnceLock::new();
        if *FAST.get_or_init(|| {
            use std::arch::x86_64::__cpuid;
            if !std::arch::is_x86_feature_detected!("bmi2") {
                return false;
            }
            // BMI2 detection implies availability of leaf 7, and thus also
            // leaves 0 and 1.
            let (vendor, signature) = (__cpuid(0), __cpuid(1).eax);
            let vendor = [vendor.ebx, vendor.edx, vendor.ecx].map(u32::to_le_bytes);
            let family = (signature >> 8) & 15;
            let family = family + if family == 15 { (signature >> 20) & 255 } else { 0 };
            fast_bmi2_vendor(vendor, family)
        }) {
            Some(Self(()))
        } else {
            None
        }
    }
}

#[cfg(target_arch = "x86_64")]
fn fast_bmi2_vendor(vendor: [[u8; 4]; 3], family: u32) -> bool {
    // AMD before Zen 3 (family 19h), Hygon, and unknown vendors use SWAR.
    // On the older AMD cores PEXT/PDEP are microcoded despite the BMI2 flag.
    vendor == [*b"Genu", *b"ineI", *b"ntel"] || (vendor == [*b"Auth", *b"enti", *b"cAMD"] && family >= 0x19)
}

impl<const BMI2: bool> IdMode<BMI2> {
    #[inline(always)]
    fn pack(self, bytes: &[u8]) -> Option<u64> {
        let n = bytes.len();
        if n == 0 || n > 9 {
            return None;
        }
        // A final NUL would be ambiguous with right padding. Internal NULs
        // are representable and significant. Reject non-ASCII before packing.
        let last = bytes[n - 1];
        if last == 0 || last > 127 {
            return None;
        }
        let word = if n >= 8 {
            u64::from_be_bytes(*bytes.first_chunk::<8>().unwrap())
        } else if n >= 4 {
            let lo = u64::from(u32::from_be_bytes(*bytes.last_chunk::<4>().unwrap()));
            let hi = u64::from(u32::from_be_bytes(*bytes.first_chunk::<4>().unwrap()));
            // Overlapping bytes are identical, so OR leaves them intact.
            (hi << 32) | (lo << ((8 - n) * 8))
        } else {
            return match bytes {
                [a] => Some(u64::from(*a) << 56),
                [a, b] => (*a < 128).then_some((u64::from(*a) << 56) | (u64::from(*b) << 49)),
                [a, b, c] => {
                    ((a | b) < 128).then_some((u64::from(*a) << 56) | (u64::from(*b) << 49) | (u64::from(*c) << 42))
                }
                _ => unreachable!(),
            };
        };
        if word & HIGH_BITS != 0 {
            return None;
        }
        let tail = if n == 9 { last } else { 0 };
        Some((self.extract(word) << 7) | u64::from(tail))
    }

    #[inline(always)]
    fn extract(self, word: u64) -> u64 {
        #[cfg(target_arch = "x86_64")]
        if BMI2 {
            let out;
            // SAFETY: the capability proves BMI2. Using inline asm here lets
            // the instruction inline through Rust's CSV callback even when
            // the callback's own codegen context lacks the target feature.
            unsafe {
                std::arch::asm!("pext {out}, {word}, {mask}",
                    out = lateout(reg) out, word = in(reg) word, mask = in(reg) LOW_BITS,
                    options(pure, nomem, nostack, preserves_flags));
            }
            return out;
        }
        // Compress unsigned byte pairs, then quads, then the entire word.
        let pairs = (word & 0x007f_007f_007f_007f) | ((word >> 1) & 0x3f80_3f80_3f80_3f80);
        let quads = (pairs & 0x0000_3fff_0000_3fff) | ((pairs >> 2) & 0x0fff_c000_0fff_c000);
        (quads & 0x0fff_ffff) | ((quads >> 4) & 0x00ff_ffff_f000_0000)
    }

    #[inline(always)]
    fn spell(self, key: u64, buf: &mut [u8; 9]) {
        #[cfg(target_arch = "x86_64")]
        if BMI2 {
            let word: u64;
            // SAFETY: the capability proves BMI2.
            unsafe {
                std::arch::asm!("pdep {out}, {packed}, {mask}",
                    out = lateout(reg) word, packed = in(reg) key >> 7, mask = in(reg) LOW_BITS,
                    options(pure, nomem, nostack, preserves_flags));
            }
            *buf.first_chunk_mut::<8>().unwrap() = word.to_be_bytes();
            buf[8] = (key & 127) as u8;
            return;
        }
        for (i, v) in buf.iter_mut().enumerate() {
            *v = ((key >> (7 * (8 - i))) & 127) as u8;
        }
    }
}

#[derive(Default)]
struct Spill<'a> {
    by_name: Map<&'a [u8], usize>,
    by_index: Vec<&'a [u8]>,
}

pub struct Codec<'a> {
    long: [RwLock<Spill<'a>>; LANES],
    hash: ahash::RandomState,
}

impl<'a> Codec<'a> {
    pub fn new() -> Self {
        Self {
            long: std::array::from_fn(|_| RwLock::default()),
            hash: rs(),
        }
    }

    #[inline(always)]
    pub fn key<const BMI2: bool>(&self, mode: IdMode<BMI2>, bytes: &'a [u8]) -> u64 {
        mode.pack(bytes).unwrap_or_else(|| self.intern(bytes))
    }

    #[inline]
    pub fn less(&self, a: u64, b: u64) -> bool {
        if (a | b) & EXTERNAL == 0 {
            a < b
        } else {
            self.bytes(a, &mut [0; 9]) < self.bytes(b, &mut [0; 9])
        }
    }

    #[cold]
    fn intern(&self, bytes: &'a [u8]) -> u64 {
        let i = (self.hash.hash_one(bytes) as usize) & (LANES - 1);
        let shard = &self.long[i];
        let known = shard.read().unwrap().by_name.get(bytes).copied();
        let n = known.unwrap_or_else(|| {
            let mut data = shard.write().unwrap();
            let Spill { by_name, by_index } = &mut *data;
            *by_name.entry(bytes).or_insert_with(|| {
                let index = by_index.len();
                by_index.push(bytes);
                index
            })
        });
        EXTERNAL | ((n as u64) << LANES.trailing_zeros()) | i as u64
    }

    #[inline]
    pub fn bytes<'b>(&self, key: u64, buf: &'b mut [u8; 9]) -> &'b [u8]
    where
        'a: 'b,
    {
        self.bytes_with(IdMode::SCALAR, key, buf)
    }

    #[inline(always)]
    pub fn bytes_with<'b, const BMI2: bool>(&self, mode: IdMode<BMI2>, key: u64, buf: &'b mut [u8; 9]) -> &'b [u8]
    where
        'a: 'b,
    {
        if key & EXTERNAL != 0 {
            let names = self.long[(key as usize) & (LANES - 1)].read().unwrap();
            return names.by_index[((key ^ EXTERNAL) >> LANES.trailing_zeros()) as usize];
        }
        mode.spell(key, buf);
        let len = 9 - (key.trailing_zeros() as usize / 7).min(9);
        &buf[..len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::links::{mix, unmix};

    // Oracle for the existing numerical representation, independent of loading
    // strategy, SWAR, machine endianness, and BMI2.
    fn original(bytes: &[u8]) -> Option<u64> {
        if bytes.is_empty() || bytes.len() > 9 || bytes.last() == Some(&0) || !bytes.is_ascii() {
            return None;
        }
        let n = bytes.iter().fold(0u64, |n, &v| (n << 7) | u64::from(v));
        Some(n << (7 * (9 - bytes.len())))
    }

    #[test]
    fn bit_for_bit_codec_contract() {
        let mut inputs = vec![(vec![0], 0)];
        // Every byte at each offset, for each inline width and adjacent spill
        // width, with both case variants and unchanged adjacent guard bytes.
        for n in 1..=17 {
            for i in 0..n {
                for v in 0..=255 {
                    let mut b = b"!aBcDeFgHiJkLmNoPqR!".to_vec();
                    b[i + 1] = v;
                    inputs.push((b, n));
                }
            }
        }
        for a in 0..=255 {
            for b in 0..=255 {
                inputs.push((vec![0xff, a, b, 0xff], 2));
            }
        }
        for n in 1..=9 {
            for fill in [0, 0x7f, 0x80, 0xff] {
                let mut b = vec![fill; n + 2];
                inputs.push((b.clone(), n));
                b[n] = 1; // including the smallest key at each width
                inputs.push((b, n));
            }
        }
        for n in [31, 32, 33, 63, 64, 65, 255, 256, 257, 4096] {
            let mut b = vec![b'x'; n + 2];
            inputs.push((b.clone(), n));
            b[n] = 0;
            inputs.push((b, n));
        }
        let codec = Codec::new();
        #[cfg(target_arch = "x86_64")]
        let bmi2 = std::arch::is_x86_feature_detected!("bmi2").then(|| IdMode::<true>(()));
        for (b, n) in &inputs {
            let input = &b[1..1 + n];
            let want = original(input);
            assert_eq!(IdMode::SCALAR.pack(input), want, "{input:?}");
            let key = codec.key(IdMode::SCALAR, input);
            assert_eq!(key & EXTERNAL == 0, want.is_some());
            assert_eq!(codec.bytes(key, &mut [0; 9]), input);
            #[cfg(target_arch = "x86_64")]
            if let Some(bmi2) = bmi2 {
                // Test the instruction even on older microcoded CPUs, as
                // long as the host actually supports it.
                assert_eq!(bmi2.pack(input), want, "{input:?}");
                assert_eq!(codec.key(bmi2, input), key);
                assert_eq!(codec.bytes_with(bmi2, key, &mut [0; 9]), input);
            }
        }
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn fast_bmi2_policy_excludes_old_amd_and_unknown_vendors() {
        let amd = [*b"Auth", *b"enti", *b"cAMD"];
        assert!(!fast_bmi2_vendor(amd, 0x17));
        assert!(fast_bmi2_vendor(amd, 0x19));
        assert!(fast_bmi2_vendor(amd, 0x1a));
        assert!(fast_bmi2_vendor([*b"Genu", *b"ineI", *b"ntel"], 6));
        assert!(!fast_bmi2_vendor([*b"Hygo", *b"nGen", *b"uine"], 0x18));
        assert!(!fast_bmi2_vendor([[0; 4]; 3], 0xff));
    }

    #[test]
    fn bytewise_order_including_prefixes_case_and_spills() {
        let mut inputs: Vec<&[u8]> = vec![
            b"\0",
            b"\0A",
            b"A",
            b"A\0",
            b"A\0Z",
            b"A ",
            b"Aa",
            b"a",
            b"a\0",
            b"a\x7f",
            b"abcdefgh",
            b"abcdefgh\0",
            b"abcdefgh1",
            b"abcdefgh10",
            b"abcdefghi",
            b"a\xc3\xa9",
            b"\xff",
        ];
        inputs.sort_unstable();
        let codec = Codec::new();
        let keys: Vec<_> = inputs.iter().map(|b| codec.key(IdMode::SCALAR, b)).collect();
        for (i, &a) in keys.iter().enumerate() {
            for (j, &b) in keys.iter().enumerate() {
                assert_eq!(codec.less(a, b), i < j);
                if (a | b) & EXTERNAL == 0 {
                    assert_eq!(a.cmp(&b), i.cmp(&j));
                }
            }
        }
    }

    #[test]
    fn every_ascii_lane_survives_routing_and_decoding() {
        let codec = Codec::new();
        // Include embedded/trailing zeros, ASCII controls, and all seven bits
        // at every lane rather than testing just generator-shaped names.
        let inputs: Vec<_> = (1..=9)
            .flat_map(|n| {
                (0..n).flat_map(move |i| {
                    (0..=127).map(move |v| {
                        let mut b = vec![b'x'; n];
                        b[i] = v;
                        b
                    })
                })
            })
            .collect();
        for input in &inputs {
            let key = codec.key(IdMode::SCALAR, input);
            assert_eq!(codec.bytes(unmix(mix(key)), &mut [0; 9]), input.as_slice());
        }
    }
}
