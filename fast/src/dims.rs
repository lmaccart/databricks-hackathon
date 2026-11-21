//! Optional direct lookup for dense decimal suffixes; the exact map is authoritative.
use crate::Map;

pub(super) struct Dimension<'a> {
    map: Map<&'a [u8], u32>,
    prefix: &'a [u8],
    dense: Vec<Option<u32>>,
}

impl<'a> Dimension<'a> {
    pub(super) fn new(map: Map<&'a [u8], u32>, first: Option<&'a [u8]>) -> Self {
        let mut result = Self {
            map,
            prefix: &[],
            dense: Vec::new(),
        };
        let Some(first) = first else { return result };
        let split = first.iter().rposition(|b| !b.is_ascii_digit()).map_or(0, |i| i + 1);
        if split == first.len() {
            return result;
        }
        result.prefix = &first[..split];
        // An input-derived bound limits memory for sparse or adversarial IDs.
        let limit = result.map.len().saturating_mul(4);
        let mut matches = Vec::new();
        let mut max = 0;
        for (&key, &value) in &result.map {
            if let Some(i) = suffix_index(key, result.prefix, limit) {
                matches.push((i, value));
                max = max.max(i);
            }
        }
        if matches.is_empty() || matches.len() < result.map.len().div_ceil(2) {
            return result;
        }
        // suffix_index requires i < limit, so max + 1 cannot overflow.
        result.dense.resize(max + 1, None);
        for (i, value) in matches {
            result.dense[i] = Some(value);
        }
        result
    }

    #[inline]
    pub(super) fn get(&self, key: &[u8]) -> Option<u32> {
        if !self.dense.is_empty()
            && let Some(i) = suffix_index(key, self.prefix, self.dense.len())
            && let Some(value) = self.dense[i]
        {
            return Some(value);
        }
        self.map.get(key).copied()
    }
}

#[inline]
fn suffix_index(key: &[u8], prefix: &[u8], limit: usize) -> Option<usize> {
    let suffix = key.strip_prefix(prefix)?;
    if suffix.is_empty() || (suffix.len() > 1 && suffix[0] == b'0') {
        return None;
    }
    let mut n = 0usize;
    for &b in suffix {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((b - b'0') as usize)?;
        if n >= limit {
            return None;
        }
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rs;

    #[test]
    fn dense_lookup_preserves_full_bytes_and_overflow_fallback() {
        let entries: &[(&[u8], u32)] = &[
            (b"p1", 1),
            (b"p2", 2),
            (b"p3", 3),
            (b"p0", 4),
            (b"p4", 10),
            (b"p01", 5),
            (b"P1", 6),
            (b"p\0", 7),
            ("pé1".as_bytes(), 8),
            (b"p999999999999999999999999", 9),
        ];
        let map = entries.iter().copied().collect::<Map<_, _>>();
        let dim = Dimension::new(map, Some(b"p1"));
        assert!(!dim.dense.is_empty());
        for &(key, value) in entries {
            assert_eq!(dim.get(key), Some(value));
        }
        for key in [&b"p001"[..], b"p5", b"p1\0", b"p", b"p999999999999999999999998"] {
            assert_eq!(dim.get(key), None);
        }
        let empty = Dimension::new(Map::with_hasher(rs()), None);
        assert_eq!(empty.get(b"p1"), None);
    }

    #[test]
    fn direct_index_matches_map_for_dense_and_sparse_inputs() {
        for prefix in [&b"p"[..], b"", b"\0", "é".as_bytes()] {
            let keys: Vec<_> = (0..200)
                .map(|i| [prefix, i.to_string().as_bytes()].concat())
                .chain((0..200).map(|i| [prefix, format!("0{i}").as_bytes()].concat()))
                .collect();
            let map: Map<&[u8], u32> = keys.iter().enumerate().map(|(i, k)| (k.as_slice(), i as u32)).collect();
            let dim = Dimension::new(map, Some(keys[0].as_slice()));
            assert!(!dim.dense.is_empty());
            for i in 0..1000 {
                for suffix in [i.to_string(), format!("0{i}"), format!("{i}\0")] {
                    let key = [prefix, suffix.as_bytes()].concat();
                    assert_eq!(dim.get(&key), dim.map.get(key.as_slice()).copied());
                }
            }
        }
        let map: Map<&[u8], u32> = [(b"p1000000000".as_slice(), 7), (b"unrelated".as_slice(), 8)]
            .into_iter()
            .collect();
        let dim = Dimension::new(map, Some(b"p1000000000"));
        assert!(dim.dense.is_empty());
        assert_eq!(dim.get(b"p1000000000"), Some(7));
        assert_eq!(dim.get(b"p1"), None);
    }
}
