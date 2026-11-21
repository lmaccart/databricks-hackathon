//! Exact unsigned sorting of packed temporal keys. Only the partition is ISA
//! specific; all other machines retain Rust's standard sort.

pub(super) fn sort_u64(keys: &mut [u64]) {
    #[cfg(all(feature = "vector-sort-model", not(target_arch = "x86_64")))]
    {
        unsafe { avx2::sort(keys) };
    }
    #[cfg(not(all(feature = "vector-sort-model", not(target_arch = "x86_64"))))]
    {
        #[cfg(target_arch = "x86_64")]
        if keys.len() >= 128 && std::arch::is_x86_feature_detected!("avx2") {
            // The feature macro includes OS support for saving vector registers.
            unsafe { avx2::sort(keys) };
            return;
        }
        keys.sort_unstable();
    }
}

#[cfg(any(target_arch = "x86_64", test, feature = "vector-sort-model"))]
mod avx2 {
    #[cfg(all(any(test, feature = "vector-sort-model"), not(target_arch = "x86_64")))]
    use emulated::*;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    // Run the same partition/control flow on non-x86 test hosts. This models
    // lane semantics, not CPU feature detection or x86 instruction execution.
    #[cfg(all(any(test, feature = "vector-sort-model"), not(target_arch = "x86_64")))]
    #[allow(non_camel_case_types, non_snake_case)]
    mod emulated {
        #[derive(Clone, Copy)]
        pub(super) struct __m256i([u64; 4]);
        pub(super) type __m256d = __m256i;

        pub(super) unsafe fn _mm256_loadu_si256(p: *const __m256i) -> __m256i {
            unsafe { p.read_unaligned() }
        }
        pub(super) fn _mm256_set1_epi64x(x: i64) -> __m256i {
            __m256i([x as u64; 4])
        }
        pub(super) fn _mm256_xor_si256(a: __m256i, b: __m256i) -> __m256i {
            __m256i(std::array::from_fn(|i| a.0[i] ^ b.0[i]))
        }
        pub(super) fn _mm256_cmpgt_epi64(a: __m256i, b: __m256i) -> __m256i {
            __m256i(std::array::from_fn(|i| {
                if (a.0[i] as i64) > (b.0[i] as i64) { u64::MAX } else { 0 }
            }))
        }
        pub(super) fn _mm256_castsi256_pd(a: __m256i) -> __m256d {
            a
        }
        pub(super) fn _mm256_movemask_pd(a: __m256d) -> i32 {
            (0..4).fold(0, |m, i| m | (((a.0[i] >> 63) as i32) << i))
        }
        pub(super) fn _mm256_permutevar8x32_epi32(a: __m256i, idx: __m256i) -> __m256i {
            let a = unsafe { std::mem::transmute::<[u64; 4], [u32; 8]>(a.0) };
            let idx = unsafe { std::mem::transmute::<[u64; 4], [u32; 8]>(idx.0) };
            let out: [u32; 8] = std::array::from_fn(|i| a[(idx[i] & 7) as usize]);
            __m256i(unsafe { std::mem::transmute::<[u32; 8], [u64; 4]>(out) })
        }
        pub(super) fn _mm256_set_epi64x(a: i64, b: i64, c: i64, d: i64) -> __m256i {
            __m256i([d as u64, c as u64, b as u64, a as u64])
        }
        pub(super) unsafe fn _mm256_maskstore_epi64(p: *mut i64, mask: __m256i, a: __m256i) {
            for i in 0..4 {
                if mask.0[i] >> 63 != 0 {
                    unsafe { p.add(i).write(a.0[i] as i64) };
                }
            }
        }
    }

    // Each mask selects the 64-bit lanes packed at the front of a vector.
    // AVX2 permutes 32-bit lanes, so the two halves of each u64 travel together.
    pub(super) const fn permutations() -> [[i32; 8]; 16] {
        let mut table = [[0; 8]; 16];
        let mut mask = 0;
        while mask < 16 {
            let mut lane = 0;
            let mut out = 0;
            while lane < 4 {
                if mask & (1 << lane) != 0 {
                    table[mask][out] = lane * 2;
                    table[mask][out + 1] = lane * 2 + 1;
                    out += 2;
                }
                lane += 1;
            }
            mask += 1;
        }
        table
    }
    const PERM: [[i32; 8]; 16] = permutations();

    // Writes exactly n elements, never overreading or writing outside either
    // slice. A pivot equal to all values is handled by the caller's fallback.
    #[cfg_attr(target_arch = "x86_64", target_feature(enable = "avx2"))]
    pub(super) unsafe fn partition(src: &[u64], dst: &mut [u64], pivot: u64) -> usize {
        unsafe {
            let n = src.len();
            let mut lo = 0;
            let mut hi = n;
            let mut i = 0;
            let sign = _mm256_set1_epi64x(i64::MIN);
            let p = _mm256_set1_epi64x((pivot ^ (1u64 << 63)) as i64);
            while i + 4 <= n {
                let v = _mm256_loadu_si256(src.as_ptr().add(i).cast());
                let cmp = _mm256_cmpgt_epi64(p, _mm256_xor_si256(v, sign));
                let mask = _mm256_movemask_pd(_mm256_castsi256_pd(cmp)) as usize;
                let left = mask.count_ones() as usize;
                let right = 4 - left;
                if left != 0 {
                    let idx = _mm256_loadu_si256(PERM[mask].as_ptr().cast());
                    let packed = _mm256_permutevar8x32_epi32(v, idx);
                    let store_mask = _mm256_set_epi64x(
                        if left > 3 { -1 } else { 0 },
                        if left > 2 { -1 } else { 0 },
                        if left > 1 { -1 } else { 0 },
                        -1,
                    );
                    _mm256_maskstore_epi64(dst.as_mut_ptr().add(lo).cast(), store_mask, packed);
                    lo += left;
                }
                if right != 0 {
                    hi -= right;
                    let idx = _mm256_loadu_si256(PERM[mask ^ 15].as_ptr().cast());
                    let packed = _mm256_permutevar8x32_epi32(v, idx);
                    let store_mask = _mm256_set_epi64x(
                        if right > 3 { -1 } else { 0 },
                        if right > 2 { -1 } else { 0 },
                        if right > 1 { -1 } else { 0 },
                        -1,
                    );
                    _mm256_maskstore_epi64(dst.as_mut_ptr().add(hi).cast(), store_mask, packed);
                }
                i += 4;
            }
            while i < n {
                let x = *src.get_unchecked(i);
                if x < pivot {
                    *dst.get_unchecked_mut(lo) = x;
                    lo += 1;
                } else {
                    hi -= 1;
                    *dst.get_unchecked_mut(hi) = x;
                }
                i += 1;
            }
            debug_assert_eq!(lo, hi);
            lo
        }
    }

    // A fixed 16-wire network; MAX pads missing lanes and also is a legal key.
    pub(super) fn small_network(a: &mut [u64]) {
        let mut v = [u64::MAX; 16];
        v[..a.len()].copy_from_slice(a);
        let mut k = 2;
        while k <= 16 {
            let mut j = k / 2;
            while j != 0 {
                for i in 0..16 {
                    let other = i ^ j;
                    if other > i && ((v[i] > v[other]) == (i & k == 0)) {
                        v.swap(i, other);
                    }
                }
                j /= 2;
            }
            k *= 2;
        }
        a.copy_from_slice(&v[..a.len()]);
    }

    fn pivot(a: &[u64]) -> u64 {
        let mut sample = [0; 9];
        for (i, x) in sample.iter_mut().enumerate() {
            // Avoid multiplication overflow even for extremely large slices.
            *x = a[(a.len() - 1) / 8 * i];
        }
        sample.sort_unstable();
        sample[4]
    }

    #[cfg_attr(target_arch = "x86_64", target_feature(enable = "avx2"))]
    unsafe fn recurse(src: &mut [u64], dst: &mut [u64], into_dst: bool, depth: u32) {
        if src.len() <= 64 || depth == 0 {
            if src.len() <= 16 {
                small_network(src);
            } else {
                src.sort_unstable();
            }
            if into_dst {
                dst.copy_from_slice(src);
            }
            return;
        }
        let p = pivot(src);
        let split = unsafe { partition(src, dst, p) };
        if split == 0 || split == src.len() {
            // This includes all-equal slices; it also guarantees progress for
            // any pathological sample or distribution of duplicates.
            src.sort_unstable();
            if into_dst {
                dst.copy_from_slice(src);
            }
            return;
        }
        let (sl, sr) = src.split_at_mut(split);
        let (dl, dr) = dst.split_at_mut(split);
        unsafe {
            recurse(dl, sl, !into_dst, depth - 1);
            recurse(dr, sr, !into_dst, depth - 1);
        }
    }

    #[cfg_attr(target_arch = "x86_64", target_feature(enable = "avx2"))]
    pub(super) unsafe fn sort(keys: &mut [u64]) {
        let mut scratch = vec![0; keys.len()];
        let depth = 2 * (usize::BITS - keys.len().leading_zeros());
        unsafe { recurse(keys, &mut scratch, false, depth) };
    }
}

#[cfg(test)]
mod tests {
    use super::{avx2, sort_u64};

    #[test]
    fn small_network_and_permutation_table() {
        for len in 0..=16 {
            for seed in 0..256u64 {
                let mut v: Vec<_> = (0..len)
                    .map(|i| (seed.wrapping_mul(0x9e3779b97f4a7c15).rotate_left(i * 3)) % 13)
                    .collect();
                let mut expected = v.clone();
                expected.sort_unstable();
                avx2::small_network(&mut v);
                assert_eq!(v, expected);
            }
        }
        let table = avx2::permutations();
        for (mask, row) in table.iter().enumerate() {
            let expected: Vec<_> = (0..4).filter(|i| mask & (1 << i) != 0).collect();
            for (out, lane) in expected.iter().enumerate() {
                assert_eq!(row[2 * out], lane * 2);
                assert_eq!(row[2 * out + 1], lane * 2 + 1);
            }
        }
    }

    #[test]
    fn partition_exact_bounds_and_masks() {
        #[cfg(target_arch = "x86_64")]
        if !std::arch::is_x86_feature_detected!("avx2") {
            eprintln!("AVX2 unavailable; direct partition test skipped");
            return;
        }
        for n in 0..=1029 {
            for seed in 0..16u64 {
                let src: Vec<u64> = (0..n)
                    .map(|i| {
                        if (seed >> (i % 4)) & 1 == 0 {
                            i as u64
                        } else {
                            u64::MAX - i as u64
                        }
                    })
                    .collect();
                let mut dst = vec![0xcafe_babe_dead_beef; n + 2];
                let split = unsafe { avx2::partition(&src, &mut dst[1..n + 1], 1 << 63) };
                assert_eq!(dst[0], 0xcafe_babe_dead_beef);
                assert_eq!(dst[n + 1], 0xcafe_babe_dead_beef);
                assert!(dst[1..1 + split].iter().all(|&x| x < 1 << 63));
                assert!(dst[1 + split..n + 1].iter().all(|&x| x >= 1 << 63));
                let mut expected = src;
                expected.sort_unstable();
                dst[1..n + 1].sort_unstable();
                assert_eq!(&dst[1..n + 1], expected);
            }
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn partition_and_sort_at_guard_page() {
        #[cfg(target_arch = "x86_64")]
        if !std::arch::is_x86_feature_detected!("avx2") {
            eprintln!("AVX2 unavailable; guard-page test skipped");
            return;
        }

        use std::ffi::{c_int, c_void};
        unsafe extern "C" {
            fn getpagesize() -> c_int;
            fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
            fn mprotect(addr: *mut c_void, len: usize, prot: c_int) -> c_int;
            fn munmap(addr: *mut c_void, len: usize) -> c_int;
        }
        #[cfg(target_os = "macos")]
        const MAP_ANON: c_int = 0x1000;
        #[cfg(target_os = "linux")]
        const MAP_ANON: c_int = 0x20;

        unsafe {
            let page = getpagesize() as usize;
            let alloc = || {
                let p = mmap(std::ptr::null_mut(), page * 3, 3, 2 | MAP_ANON, -1, 0);
                assert_ne!(p as isize, -1);
                assert_eq!(mprotect(p, page, 0), 0);
                assert_eq!(mprotect(p.cast::<u8>().add(page * 2).cast(), page, 0), 0);
                p.cast::<u8>()
            };
            let a = alloc();
            let b = alloc();
            for n in 1..=(page / 8).min(513) {
                let input = std::slice::from_raw_parts_mut(a.add(page * 2 - n * 8).cast::<u64>(), n);
                let output = std::slice::from_raw_parts_mut(b.add(page * 2 - n * 8).cast::<u64>(), n);
                for (i, x) in input.iter_mut().enumerate() {
                    *x = (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
                }
                let mut expected = input.to_vec();
                expected.sort_unstable();
                let split = avx2::partition(input, output, 1 << 63);
                assert!(output[..split].iter().all(|&x| x < 1 << 63));
                assert!(output[split..].iter().all(|&x| x >= 1 << 63));
                avx2::sort(input);
                assert_eq!(input, expected);
            }
            assert_eq!(munmap(a.cast(), page * 3), 0);
            assert_eq!(munmap(b.cast(), page * 3), 0);
        }
    }

    fn check(data: &[u64]) {
        let mut expected = data.to_vec();
        expected.sort_unstable();
        // Offset the input by one element and leave canaries on both sides.
        let mut actual = vec![0xdead_beef_dead_beef];
        actual.extend_from_slice(data);
        actual.push(0xcafe_babe_cafe_babe);
        let end = actual.len() - 1;
        sort_u64(&mut actual[1..end]);
        assert_eq!(actual[0], 0xdead_beef_dead_beef);
        assert_eq!(actual[end], 0xcafe_babe_cafe_babe);
        assert_eq!(&actual[1..end], expected);
        #[cfg(not(target_arch = "x86_64"))]
        {
            // Exercise the vector algorithm through the test-only lane model.
            actual[1..end].copy_from_slice(data);
            unsafe { avx2::sort(&mut actual[1..end]) };
            assert_eq!(&actual[1..end], expected);
            assert_eq!(actual[0], 0xdead_beef_dead_beef);
            assert_eq!(actual[end], 0xcafe_babe_cafe_babe);
        }
    }

    #[test]
    fn u64_orders_adversarial_inputs() {
        let mut state = 0x1234_5678_90ab_cdefu64;
        for &n in &[
            0, 1, 2, 3, 4, 7, 15, 16, 17, 31, 32, 33, 63, 127, 255, 511, 512, 513, 1023, 4095, 4096, 4097, 65537,
        ] {
            let asc: Vec<_> = (0..n).map(|x| x as u64).collect();
            check(&asc);
            check(&asc.iter().rev().copied().collect::<Vec<_>>());
            check(&vec![u64::MAX; n]);
            check(
                &(0..n)
                    .map(|i| if i % 7 == 0 { 0 } else { u64::MAX })
                    .collect::<Vec<_>>(),
            );
            check(&(0..n).map(|i| (i % 3) as u64).collect::<Vec<_>>());
            check(
                &(0..n)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        state
                    })
                    .collect::<Vec<_>>(),
            );
        }
        check(&[0, u64::MAX, 1 << 63, (1 << 63) - 1, 0, u64::MAX]);
    }
}
