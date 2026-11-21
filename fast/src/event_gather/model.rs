//! Safe deterministic model of the x86 operations, using the same row layout,
//! decoder and dispatch. Never selected by a normal production build.
use super::*;

pub(super) struct Model;

impl Kernel for Model {
    type Block = [u8; 128];

    fn load(data: &[u8; 128]) -> (Self::Block, u128, u128, u128) {
        let mask = |b| {
            data.iter()
                .enumerate()
                .fold(0, |m, (i, &x)| m | (u128::from(x == b) << i))
        };
        (*data, mask(b'\n'), mask(b','), mask(b'"'))
    }

    fn deposit(bits: u64, mut mask: u64) -> u64 {
        let mut out = 0;
        let mut bits = bits;
        while mask != 0 {
            let lsb = mask & mask.wrapping_neg();
            if bits & 1 != 0 {
                out |= lsb;
            }
            bits >>= 1;
            mask ^= lsb;
        }
        out
    }

    fn extract(bits: u64, mut mask: u64) -> u64 {
        let mut out = 0;
        let mut i = 0;
        while mask != 0 {
            let lsb = mask & mask.wrapping_neg();
            if bits & lsb != 0 {
                out |= 1 << i;
            }
            i += 1;
            mask ^= lsb;
        }
        out
    }

    fn gather(block: Self::Block, starts: [u8; 4], keep: u64, f3: bool) -> Gathered {
        let layout = if f3 { F3_LAYOUT } else { LAYOUT };
        let mut words = [0; 8];
        let mut terms = [0; 8];
        let mut digit_mask = 0;
        let mut syntax_mask = 0;
        for w in 0..8 {
            let offs = layout[w].to_le_bytes();
            let base = BASES[w].to_le_bytes();
            let mut bytes = [0; 8];
            for j in 0..8 {
                let bit = 1u64 << (w * 8 + j);
                let idx = usize::from(starts[usize::from(base[j])].wrapping_add(offs[j]) & 127);
                let x = if keep & bit != 0 {
                    block[idx]
                } else if f3 && F3_PAD & bit != 0 {
                    b'0'
                } else {
                    0
                };
                bytes[j] = x;
                if x.is_ascii_digit() {
                    digit_mask |= bit;
                }
                if x == SYNTAX.to_le_bytes()[j] {
                    syntax_mask |= bit;
                }
            }
            words[w] = u64::from_le_bytes(bytes);
            let pairs: [u16; 4] = std::array::from_fn(|k| {
                u16::from(bytes[k * 2].wrapping_sub(b'0')) * 10 + u16::from(bytes[k * 2 + 1].wrapping_sub(b'0'))
            });
            terms[w] = if f3 && (w == 3 || w == 5) {
                let quad = |k: usize| u64::from(pairs[k]) * 100 + u64::from(pairs[k + 1]);
                quad(0) | (quad(2) << 32)
            } else {
                pairs
                    .iter()
                    .enumerate()
                    .fold(0, |m, (i, &x)| m | (u64::from(x) << (16 * i)))
            };
        }
        if f3 {
            words[3] = terms[3];
        }
        Gathered {
            words,
            pairs: [terms[4], terms[5]],
            digit_mask,
            syntax_mask,
        }
    }
}
