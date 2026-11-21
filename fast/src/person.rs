//! An exact person key, cheap enough to carry through both sorts. Nine ASCII
//! bytes fit in 63 bits. Every other spelling uses a collision-checked interner;
//! no hash (or truncated hash) is ever accepted as proof of identity.
use crate::{Map, rs};
use std::sync::RwLock;

const HIGH_BITS: u64 = 0x8080_8080_8080_8080;
const EXTERNAL: u64 = 1 << 63;
const LANES: usize = 64;

/// A capability obtained only after runtime CPU detection. The const parameter
/// specializes the surrounding chunk loop, not a function pointer per ID.
#[derive(Clone, Copy)]
pub(crate) struct IdMode<const BMI2: bool>(());

impl IdMode<false> {
    pub(crate) const SCALAR: Self = Self(());
}

#[cfg(target_arch = "x86_64")]
impl IdMode<true> {
    pub(crate) fn detect() -> Option<Self> {
        static FAST: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *FAST.get_or_init(|| {
            use std::arch::x86_64::__cpuid;
            if !std::arch::is_x86_feature_detected!("bmi2") {
                return false;
            }
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

    #[cfg(test)]
    pub(crate) fn supported_for_test() -> Option<Self> {
        std::arch::is_x86_feature_detected!("bmi2").then_some(Self(()))
    }
}

#[cfg(target_arch = "x86_64")]
fn fast_bmi2_vendor(vendor: [[u8; 4]; 3], family: u32) -> bool {
    // Old AMD and Hygon have microcoded PEXT/PDEP; unknown vendors are conservative.
    vendor == [*b"Genu", *b"ineI", *b"ntel"] || (vendor == [*b"Auth", *b"enti", *b"cAMD"] && family >= 0x19)
}

impl<const BMI2: bool> IdMode<BMI2> {
    #[inline(always)]
    fn inline(self, bytes: &[u8]) -> Option<u64> {
        let n = bytes.len();
        if n == 0 || n > 9 {
            return None;
        }
        // A final NUL is ambiguous with padding. Internal NULs remain significant.
        let last = bytes[n - 1];
        if last == 0 || last > 127 {
            return None;
        }
        let word = if n >= 8 {
            u64::from_be_bytes(*bytes.first_chunk::<8>().unwrap())
        } else if n >= 4 {
            let lo = u64::from(u32::from_be_bytes(*bytes.last_chunk::<4>().unwrap()));
            let hi = u64::from(u32::from_be_bytes(*bytes.first_chunk::<4>().unwrap()));
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
        Some(self.pack_inline(word, if n == 9 { last } else { 0 }))
    }

    /// The caller has proved eight big-endian ASCII bytes and a ninth byte.
    #[inline(always)]
    pub(crate) fn pack_inline(self, word: u64, tail: u8) -> u64 {
        #[cfg(target_arch = "x86_64")]
        if BMI2 {
            let eight: u64;
            // SAFETY: only a runtime-checked capability can select this mode.
            unsafe {
                std::arch::asm!("pext {out}, {word}, {mask}",
                    out = lateout(reg) eight, word = in(reg) word, mask = in(reg) !HIGH_BITS,
                    options(pure, nomem, nostack, preserves_flags));
            }
            return (eight << 7) | u64::from(tail);
        }
        let pairs = (word & 0x007f_007f_007f_007f) | ((word >> 1) & 0x3f80_3f80_3f80_3f80);
        let quads = (pairs & 0x0000_3fff_0000_3fff) | ((pairs >> 2) & 0x0fff_c000_0fff_c000);
        let eight = (quads & 0x0fff_ffff) | ((quads >> 4) & 0x00ff_ffff_f000_0000);
        (eight << 7) | u64::from(tail)
    }

    #[inline(always)]
    fn spell(self, key: u64, buf: &mut [u8; 9]) {
        #[cfg(target_arch = "x86_64")]
        if BMI2 {
            let word: u64;
            // SAFETY: only a runtime-checked capability can select this mode.
            unsafe {
                std::arch::asm!("pdep {out}, {packed}, {mask}",
                    out = lateout(reg) word, packed = in(reg) key >> 7, mask = in(reg) !HIGH_BITS,
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

#[cfg(test)]
#[inline]
pub fn inline(bytes: &[u8]) -> Option<u64> {
    IdMode::SCALAR.inline(bytes)
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

    #[inline]
    pub fn key(&self, bytes: &'a [u8]) -> u64 {
        self.key_with(IdMode::SCALAR, bytes)
    }

    #[inline(always)]
    pub fn key_with<const BMI2: bool>(&self, mode: IdMode<BMI2>, bytes: &'a [u8]) -> u64 {
        mode.inline(bytes).unwrap_or_else(|| self.intern(bytes))
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

    fn oracle(bytes: &[u8]) -> Option<u64> {
        if bytes.is_empty() || bytes.len() > 9 || bytes.last() == Some(&0) || !bytes.is_ascii() {
            return None;
        }
        Some(bytes.iter().fold(0u64, |key, &v| (key << 7) | u64::from(v)) << (7 * (9 - bytes.len())))
    }

    #[test]
    fn short_copy_and_both_codecs_are_byte_exact() {
        let mut inputs = vec![vec![]];
        for n in 1..=17 {
            for pos in 0..n {
                for byte in 0..=255 {
                    let mut b = vec![b'x'; n];
                    b[pos] = byte;
                    inputs.push(b);
                }
            }
        }
        for a in 0..=255 {
            for b in 0..=255 {
                inputs.push(vec![a, b]);
            }
        }
        for n in [31, 32, 33, 63, 64, 65, 255, 256, 257, 4096] {
            inputs.push(vec![b'x'; n]);
            let mut trailing_nul = vec![b'x'; n];
            trailing_nul[n - 1] = 0;
            inputs.push(trailing_nul);
        }
        let codec = Codec::new();
        for b in &inputs {
            let want = oracle(b);
            assert_eq!(IdMode::SCALAR.inline(b), want, "{b:?}");
            let key = codec.key(b);
            assert_eq!(key & EXTERNAL == 0, want.is_some(), "{b:?}");
            assert_eq!(codec.bytes(key, &mut [0; 9]), b);
            #[cfg(target_arch = "x86_64")]
            if let Some(mode) = IdMode::supported_for_test() {
                assert_eq!(mode.inline(b), want, "{b:?}");
                assert_eq!(codec.key_with(mode, b), key);
                assert_eq!(codec.bytes_with(mode, key, &mut [0; 9]), b);
            }
        }
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn bmi2_policy_rejects_microcoded_amd_and_unknown_vendors() {
        let amd = [*b"Auth", *b"enti", *b"cAMD"];
        assert!(!fast_bmi2_vendor(amd, 0x17));
        assert!(fast_bmi2_vendor(amd, 0x19));
        assert!(fast_bmi2_vendor(amd, 0x1a));
        assert!(fast_bmi2_vendor([*b"Genu", *b"ineI", *b"ntel"], 6));
        assert!(!fast_bmi2_vendor([*b"Hygo", *b"nGen", *b"uine"], 0x18));
        assert!(!fast_bmi2_vendor([[0; 4]; 3], 0xff));
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
            let key = codec.key(input);
            assert_eq!(codec.bytes(unmix(mix(key)), &mut [0; 9]), input.as_slice());
        }
    }
}
