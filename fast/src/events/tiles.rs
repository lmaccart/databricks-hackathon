//! Chunk-owned storage for append-only sharded streams. Each shard gets a
//! small first extent; only a full extent claims another tile from the pool.
use crate::NS;
use std::mem::MaybeUninit;

pub(super) const FIRST: usize = 64;
pub(super) const TILE: usize = 128;
const FIRST_LEN: usize = FIRST * NS;
const GROUP: usize = TILE * 32;
const END: u32 = u32::MAX;

struct Cursor {
    write: u32,
    end: u32,
    head: u32,
    len: u32,
    dead: u32,
}

pub(super) struct Tiles<T> {
    cursors: [Cursor; NS],
    first: Box<[MaybeUninit<T>]>,
    blocks: Vec<Box<[MaybeUninit<T>]>>,
    next: Vec<u32>,
}

impl<T: Copy> Tiles<T> {
    pub fn new() -> Self {
        Self {
            cursors: std::array::from_fn(|shard| Cursor {
                write: (shard * FIRST) as u32,
                end: ((shard + 1) * FIRST) as u32,
                head: END,
                len: 0,
                dead: 0,
            }),
            first: Box::new_uninit_slice(0),
            blocks: Vec::new(),
            next: Vec::new(),
        }
    }

    #[cold]
    fn allocate_first(&mut self) {
        self.first = Box::new_uninit_slice(FIRST_LEN);
    }

    #[cold]
    fn extend(&mut self, shard: usize) {
        let page = self.next.len();
        let offset = page * TILE;
        if offset.is_multiple_of(GROUP) {
            self.blocks.push(Box::new_uninit_slice(GROUP));
        }
        self.next.push(END);
        let c = &mut self.cursors[shard];
        if c.head == END {
            c.head = page as u32;
        } else {
            self.next[(c.end as usize - FIRST_LEN) / TILE - 1] = page as u32;
        }
        c.write = (FIRST_LEN + offset) as u32;
        c.end = c.write + TILE as u32;
    }

    #[inline]
    pub fn push(&mut self, shard: usize, row: T) -> usize {
        if self.cursors[shard].write == self.cursors[shard].end {
            self.extend(shard);
        }
        let c = &mut self.cursors[shard];
        let slot = c.write as usize;
        c.write += 1;
        c.len += 1;
        self.write(slot, row);
        slot
    }

    #[inline]
    fn write(&mut self, slot: usize, row: T) {
        if slot < FIRST_LEN {
            if self.first.is_empty() {
                self.allocate_first();
            }
            self.first[slot].write(row);
        } else {
            let offset = slot - FIRST_LEN;
            self.blocks[offset / GROUP][offset % GROUP].write(row);
        }
    }

    // Dedup supplies an exact slot returned by push, at most once per row.
    pub fn mark_dead(&mut self, shard: usize, slot: usize, marker: T) {
        self.write(slot, marker);
        self.cursors[shard].dead += 1;
    }

    pub fn len(&self, shard: usize) -> usize {
        let c = &self.cursors[shard];
        (c.len - c.dead) as usize
    }

    pub fn has_dead(&self, shard: usize) -> bool {
        self.cursors[shard].dead != 0
    }

    pub fn slices(&self, shard: usize) -> impl Iterator<Item = &[T]> {
        let c = &self.cursors[shard];
        let first = (c.len != 0).then(|| {
            let start = shard * FIRST;
            // SAFETY: The first min(len, FIRST) slots were written by push.
            unsafe { initialized(&self.first[start..start + FIRST.min(c.len as usize)]) }
        });
        let mut left = (c.len as usize).saturating_sub(FIRST);
        let mut page = c.head;
        first.into_iter().chain(std::iter::from_fn(move || {
            if page == END {
                return None;
            }
            let offset = page as usize * TILE;
            let count = left.min(TILE);
            let start = offset % GROUP;
            left -= count;
            page = self.next[page as usize];
            // SAFETY: Tiles are linked only after the previous extent fills.
            // `left` bounds this tile's prefix by the actual number of appends.
            Some(unsafe { initialized(&self.blocks[offset / GROUP][start..start + count]) })
        }))
    }
}

// SAFETY: Every element in `rows` must have been initialized.
unsafe fn initialized<T: Copy>(rows: &[MaybeUninit<T>]) -> &[T] {
    unsafe { std::slice::from_raw_parts(rows.as_ptr().cast(), rows.len()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleaved_tiles_keep_stable_locations_across_block_allocations() {
        let mut tiles = Tiles::new();
        let count = FIRST + 19 * TILE + 3;
        let mut slots = Vec::new();
        assert!(tiles.slices(2).next().is_none());
        for i in 0..count {
            tiles.push(NS - 1, !i);
            slots.push(tiles.push(2, i));
        }
        assert!(tiles.slices(0).next().is_none());
        for i in [0, FIRST - 1, FIRST, FIRST + TILE, count - 1] {
            tiles.mark_dead(2, slots[i], usize::MAX);
        }
        assert_eq!(tiles.len(2), count - 5);
        assert_eq!(tiles.len(NS - 1), count);
        assert!(tiles.has_dead(2));
        assert!(!tiles.has_dead(NS - 1));
        assert!(tiles.slices(NS - 1).flatten().copied().eq((0..count).map(|i| !i)));
        for (i, &v) in tiles.slices(2).flatten().enumerate() {
            if [0, FIRST - 1, FIRST, FIRST + TILE, count - 1].contains(&i) {
                assert_eq!(v, usize::MAX);
            } else {
                assert_eq!(v, i);
            }
        }
    }
}
