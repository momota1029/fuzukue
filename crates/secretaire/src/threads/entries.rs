//! The entries of one transcript, with change marks for the sealed prefix.
//!
//! The list is private to this module. Reads go through the slice, and every
//! change goes through `entry_mut`, so a sealed entry cannot change without
//! being compared against its original before the next save.

use std::collections::BTreeSet;
use std::ops::Deref;

use anyhow::Result;

use super::{segments::Segment, Entry};

pub(super) type Digest = [u8; 32];

const PARALLEL_COMPARISONS: usize = 1024;

#[derive(Default)]
pub(super) struct Entries {
    items: Vec<Entry>,
    sealed: Option<Box<Sealed>>,
}

struct Sealed {
    segments: Vec<Segment>,
    /// One digest of the bytes stored in a segment for each sealed entry.
    originals: Vec<Digest>,
    touched: BTreeSet<usize>,
    overrides: BTreeSet<usize>,
}

impl Deref for Entries {
    type Target = [Entry];

    fn deref(&self) -> &[Entry] {
        &self.items
    }
}

impl Entries {
    pub(super) fn push(&mut self, entry: Entry) {
        self.items.push(entry);
    }

    pub(super) fn entry_mut(&mut self, position: usize) -> &mut Entry {
        if let Some(sealed) = &mut self.sealed {
            if position < sealed.originals.len() {
                sealed.touched.insert(position);
            }
        }
        &mut self.items[position]
    }

    /// Withdraws the newest entry. Sealed entries can never be withdrawn.
    pub(super) fn pop_latest(&mut self) -> Option<Entry> {
        assert!(self.items.len() > self.sealed_len(), "cannot remove a sealed transcript entry");
        self.items.pop()
    }

    pub(super) fn into_vec(self) -> Vec<Entry> {
        self.items
    }

    /// Mark the first `originals.len()` entries as stored in `segments`.
    /// `replaced` names the positions whose current value came from the latest file.
    pub(super) fn seal(&mut self, segments: Vec<Segment>, originals: Vec<Digest>, replaced: BTreeSet<usize>) {
        assert!(originals.len() <= self.items.len());
        self.sealed = Some(Box::new(Sealed {
            segments, originals, touched: replaced.clone(), overrides: replaced,
        }));
    }

    /// Model a split in memory before its new segment files exist.
    pub(super) fn extend_sealed_prefix(&mut self, originals: Vec<Digest>) {
        let sealed = self.sealed.get_or_insert_with(|| Box::new(Sealed {
            segments: Vec::new(),
            originals: Vec::new(),
            touched: BTreeSet::new(),
            overrides: BTreeSet::new(),
        }));
        sealed.originals.extend(originals);
        assert!(sealed.originals.len() <= self.items.len());
    }

    pub(super) fn is_sealed(&self) -> bool {
        self.sealed.is_some()
    }

    pub(super) fn sealed_len(&self) -> usize {
        self.sealed.as_ref().map_or(0, |sealed| sealed.originals.len())
    }

    pub(super) fn segments(&self) -> &[Segment] {
        self.sealed.as_ref().map_or(&[], |sealed| sealed.segments.as_slice())
    }

    /// Positions of sealed entries that the latest file must carry in full.
    /// Current after `refresh_overrides` succeeds.
    pub(super) fn overrides(&self) -> Vec<usize> {
        self.sealed.as_ref().map_or_else(Vec::new, |sealed| sealed.overrides.iter().copied().collect())
    }

    /// Compare only the sealed entries that were touched. An entry that fails
    /// to serialize stays marked for the next attempt.
    pub(super) fn refresh_overrides(&mut self, digest: impl Fn(&Entry) -> Result<Digest> + Sync) -> Result<()> {
        let Some(sealed) = &mut self.sealed else { return Ok(()) };
        let positions: Vec<usize> = sealed.touched.iter().copied().collect();
        let (items, originals) = (&self.items, &sealed.originals);
        let compare = |chunk: &[usize]| -> Vec<(usize, Result<bool>)> {
            chunk.iter().map(|&position| {
                (position, digest(&items[position]).map(|digest| digest != originals[position]))
            }).collect()
        };
        // A cold load touches most of the history. Saves touch little of it.
        let results = if positions.len() < PARALLEL_COMPARISONS {
            compare(&positions)
        } else {
            std::thread::scope(|scope| {
                let workers: Vec<_> = positions.chunks(positions.len().div_ceil(super::segments::parallelism()))
                    .map(|chunk| scope.spawn(|| compare(chunk))).collect();
                workers.into_iter().flat_map(|worker| worker.join().expect("entry comparison")).collect()
            })
        };
        let mut failure = None;
        for (position, changed) in results {
            match changed {
                Ok(changed) => {
                    if changed {
                        sealed.overrides.insert(position);
                    } else {
                        sealed.overrides.remove(&position);
                    }
                    sealed.touched.remove(&position);
                }
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }
}
