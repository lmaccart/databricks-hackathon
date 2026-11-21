//! Bounded input sampling selects exact aliases for the direct physical route.
use super::*;

const SAMPLE_CHUNKS: usize = 128;
const SAMPLE_BYTES: usize = 4096;
const SLOTS: usize = 2048;

#[derive(Clone, Copy, Default)]
struct Entry {
    hash: u64,
    routed: u64,
}

pub(super) struct Hot {
    entries: Vec<Entry>,
}

impl Hot {
    pub fn sample<'a>(events: &[Chunk<'a>], clicks: &[Chunk<'a>], people: &People<'a>, enabled: bool) -> Self {
        let mut entries = vec![Entry::default(); SLOTS];
        if !enabled {
            return Self { entries };
        }
        let mut chunks: Vec<_> = events.iter().chain(clicks).collect();
        chunks.sort_unstable_by_key(|c| (c.source, c.data.as_ptr() as usize));
        let mut files: Vec<&[&Chunk<'a>]> = Vec::new();
        let mut begin = 0;
        while begin < chunks.len() {
            let mut end = begin + 1;
            while end < chunks.len() && chunks[end].source == chunks[begin].source {
                end += 1;
            }
            files.push(&chunks[begin..end]);
            begin = end;
        }
        let mut counts = Map::with_hasher(rs());
        let nfiles = files.len().min(SAMPLE_CHUNKS);
        for i in 0..nfiles {
            let file = files[i * files.len() / nfiles];
            let budget = (SAMPLE_CHUNKS / nfiles + usize::from(i < SAMPLE_CHUNKS % nfiles)).min(file.len());
            for j in 0..budget {
                let chunk = file[j * file.len() / budget];
                let mut prefix = *chunk;
                if chunk.data.len() > SAMPLE_BYTES {
                    let bytes = &chunk.data[..SAMPLE_BYTES];
                    prefix.data = match memchr::memrchr(b'\n', bytes) {
                        Some(end) => &bytes[..end + 1],
                        None => &[],
                    };
                }
                for_rows(&prefix, |row| {
                    if !row[1].is_empty() {
                        let hash = links::mix(people.codec.key(row[1]));
                        *counts.entry(hash).or_insert(0usize) += 1;
                    }
                });
            }
        }
        let mut candidates: Vec<_> = counts.into_iter().filter(|(_, count)| *count >= 2).collect();
        candidates.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        for (hash, _) in candidates {
            let entry = &mut entries[hash as usize & (SLOTS - 1)];
            if entry.hash == 0
                && let Identity::Linked(routed) = people.lookup(links::unmix(hash), hash)
            {
                *entry = Entry { hash, routed };
            }
        }
        Self { entries }
    }

    #[inline]
    pub fn lookup(&self, hash: u64) -> Option<u64> {
        let entry = self.entries[hash as usize & (SLOTS - 1)];
        (entry.hash == hash && hash != 0).then_some(entry.routed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_exact_opaque_tokens_and_rejects_malformed_rows_and_collisions() {
        let link = "user_id_a,user_id_b\né-long-user,canonical\nbad,other\n";
        let lc = chunks_of(link.as_bytes(), &["user_id_a", "user_id_b"], Path::new("links"));
        let people = person_names(&lc);
        let data = concat!(
            "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\n",
            "a,é-long-user,t,i,view,p,\n",
            "b,\"é-long-user\",t,i,view,p,\n",
            "c,bad,t,i,view,p,\"broken\n",
            "d,bad,t,i,view,p,\"broken\n",
            "e,canonical,t,i,view,p,\n",
        );
        let cols = [
            "event_id",
            "user_id",
            "ts",
            "ingest_ts",
            "event_type",
            "product_id",
            "price_cents",
        ];
        let chunks = chunks_of(data.as_bytes(), &cols, Path::new("events"));
        let hot = Hot::sample(&chunks, &[], &people, true);
        let hash = links::mix(people.codec.key("é-long-user".as_bytes()));
        assert!(hot.lookup(hash).is_some());
        assert!(hot.lookup(links::mix(people.codec.key(b"bad"))).is_none());
        assert!(hot.lookup(hash ^ SLOTS as u64).is_none());
        let disabled = Hot::sample(&chunks, &[], &people, false);
        assert!(disabled.lookup(hash).is_none());
        let empty = Hot::sample(&[], &[], &people, true);
        assert!(empty.lookup(hash).is_none());
    }
}
